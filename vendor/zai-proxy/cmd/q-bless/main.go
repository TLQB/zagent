// cmd/q-bless/main.go — bless a fresh Q in ~30-45s of Chromium, then exit.
//
// Research 2026-09-03 (EXP_CAPTCHA_RESULTS.md §10) proved the Aliyun server
// attaches a per-Q trust score at Log1 time based on the CONNECTING CLIENT's
// fingerprint (Chrome TLS + HTTP/2 frame ordering). A Q issued to real Chrome
// lets pure Go mint unlimited tokens on it for HOURS (verified ≥3h15m,
// 3/3 PASS); a Q issued to any Go/curl/Node client is born poisoned (F001
// forever). Hence "bless": let Chrome register the Q once, then do everything
// else in Go.
//
// Per run:
//  1. Launch Chromium headless, load chat.z.ai, capture the Log1
//     DeviceConfig response (browser-context network — this is what earns
//     the trust score).
//  2. Decrypt DC (keyWDC) → {Q, sessionKey, serverIP, qts}.
//  3. Mint ONE canary Token-A on the new Q and verify it via
//     VerifyCaptchaV3 (bridge creds, pure Go). PASS ⇒ the Q is PROVEN
//     blessed right now, not guessed.
//  4. Write qbless.json atomically. Exit. Chrome dies with us.
//
// The bridge (internal/zbridge/qmint.go) reads qbless.json and mints
// unlimited tokens on the blessed Q — zero browser, zero DB.
//
// Schedule with cron / systemd timer, e.g. every 2h:
//
//	0 */2 * * * /usr/local/bin/qbless -out /var/lib/zai/qbless.json
package main

import (
	"bytes"
	"compress/zlib"
	"crypto/aes"
	"crypto/cipher"
	"crypto/hmac"
	"crypto/md5"
	"crypto/rand"
	"crypto/sha1"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/mxschmitt/playwright-go"
)

// ============================================================================
// Protocol constants — mirror scripts/research/nosdk/gomint.go (verified
// live 2026-09-02/03; static across sessions).
// ============================================================================

const (
	ivString = "0123456789ABCDEF" // fixed IV for every AES call

	cloudauthURL = "https://cloudauth-device-dualstack.ap-southeast-1.aliyuncs.com/"

	feilinVersion = "W20220202"
	appKey        = "3795d28242a11619bc25f786f84e53d4"
	sceneID       = "didk33e0"
	region        = "sgp"
	endpoint      = "no8xfe"
	clientID      = "captcha-front"
	safApp        = "saf-captcha"

	captchaOpenInitURL   = "https://no8xfe.captcha-open-southeast.aliyuncs.com/"
	captchaOpenVerifyURL = "https://no8xfe-verify.captcha-open-southeast.aliyuncs.com/"
)

// Credentials are placeholders in the public source. Real values are injected
// at link time (-ldflags -X) by scripts/build-zai-proxy-sidecar.sh from the
// environment or scripts/.zai-proxy-secrets.env locally, and from GitHub
// Actions secrets in CI. They must be vars (not consts) for -X to apply.
var (
	keyWWrap = "REDACTED00000000" // Log1 uuid-wrap payload
	keyWDC   = "REDACTED00000000" // decrypts the server's DeviceConfig blob

	duaneID     = "REDACTED000000000000000"      // cloudauth AccessKeyId
	duaneSecret = "REDACTED00000000000000000000" // + "&" appended for HMAC

	// VerifyCaptchaV3 uses the bridge (cloudauth) credential pair — same as
	// bridge-style verify yields F001 — certifyIDs are credential-scoped.
	bridgeAccessKey = "REDACTED000000000000000"
	bridgeSecretKey = "REDACTED000000000000000000000000"
)

// ============================================================================
// Session file — consumed by internal/zbridge/qmint.go
// ============================================================================

// BlessSession is one blessed Q + the identity fields tokens mint on it.
type BlessSession struct {
	Q          string `json:"q"`
	SK         string `json:"sk"` // sessionKey (raw ASCII, NOT b64)
	QTS        int64  `json:"qts"`
	IP         string `json:"ip"`
	SessA      string `json:"sessA"`      // w[71] — session-stable
	SessB      string `json:"sessB"`      // w[73]
	BootTS     int64  `json:"bootTS"`     // w[72]
	DeviceTag  string `json:"deviceTag"`  // w[78]
	Verified   bool   `json:"verified"`   // canary PASSed at bless time
	VerifiedAt int64  `json:"verifiedAt"` // unix ms of the canary PASS
	Canary     string `json:"canary"`     // token that PASSed (audit)
	BlessedAt  int64  `json:"blessedAt"`  // unix ms of Log1
}

// ============================================================================
// Crypto helpers (CryptoJS mirror: AES-128-CBC + PKCS7, fixed IV)
// ============================================================================

func aesCBCEncrypt(key, plaintext string) (string, error) {
	block, err := aes.NewCipher([]byte(key))
	if err != nil {
		return "", err
	}
	pt := []byte(plaintext)
	padLen := aes.BlockSize - len(pt)%aes.BlockSize
	pt = append(pt, bytes.Repeat([]byte{byte(padLen)}, padLen)...)
	ct := make([]byte, len(pt))
	cipher.NewCBCEncrypter(block, []byte(ivString)).CryptBlocks(ct, pt)
	return base64.StdEncoding.EncodeToString(ct), nil
}

// decryptDeviceConfig: AES-128-CBC(keyWDC) → (hex-string | plain) →
// b64(sessionKey)#…#Q#feilinURL##ts#ip#0 (two live layouts, both handled).
func decryptDeviceConfig(dcB64 string) (q, sk, ip string, qts int64, err error) {
	ct, err := base64.StdEncoding.DecodeString(dcB64)
	if err != nil {
		return
	}
	if len(ct)%aes.BlockSize != 0 || len(ct) == 0 {
		err = fmt.Errorf("DeviceConfig ct %db not block-aligned", len(ct))
		return
	}
	block, err := aes.NewCipher([]byte(keyWDC))
	if err != nil {
		return
	}
	pt := make([]byte, len(ct))
	cipher.NewCBCDecrypter(block, []byte(ivString)).CryptBlocks(pt, ct)
	n := int(pt[len(pt)-1])
	if n == 0 || n > aes.BlockSize || n > len(pt) {
		err = fmt.Errorf("bad PKCS7 padding %d", n)
		return
	}
	pt = pt[:len(pt)-n]
	raw, herr := hex.DecodeString(string(pt))
	if herr != nil {
		raw = pt
	}
	fields := strings.Split(string(raw), "#")
	if len(fields) < 8 {
		err = fmt.Errorf("DeviceConfig has %d fields, want ≥8", len(fields))
		return
	}
	skb, serr := base64.StdEncoding.DecodeString(fields[0])
	if serr != nil {
		err = fmt.Errorf("sessionKey b64: %v", serr)
		return
	}
	q = fields[2]
	ipField := 8
	if v, e := strconv.ParseInt(fields[6], 10, 64); e == nil && v > 1e12 {
		qts, ipField = v, 7
	} else if v, e := strconv.ParseInt(fields[7], 10, 64); e == nil && v > 1e12 {
		qts = v
	} else if hh := strings.SplitN(q, "-h-", 2); len(hh) == 2 {
		if tsStr := strings.SplitN(hh[1], "-", 2); len(tsStr) == 2 {
			if v, e := strconv.ParseInt(tsStr[0], 10, 64); e == nil {
				qts = v
			}
		}
	}
	ip = fields[ipField]
	sk = string(skb)
	return
}

// ============================================================================
// Aliyun RPC signing (double url-encode style)
// ============================================================================

func rpcSign(params map[string]string, secret string) string {
	keys := make([]string, 0, len(params))
	for k := range params {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var canonical strings.Builder
	for i, k := range keys {
		if i > 0 {
			canonical.WriteByte('&')
		}
		canonical.WriteString(url.QueryEscape(k))
		canonical.WriteByte('=')
		canonical.WriteString(url.QueryEscape(params[k]))
	}
	stringToSign := "POST&" + url.QueryEscape("/") + "&" + url.QueryEscape(canonical.String())
	mac := hmac.New(sha1.New, []byte(secret+"&"))
	mac.Write([]byte(stringToSign))
	return base64.StdEncoding.EncodeToString(mac.Sum(nil))
}

func rpcPostForm(endpointURL string, params map[string]string) (string, error) {
	keys := make([]string, 0, len(params))
	for k := range params {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var body strings.Builder
	for i, k := range keys {
		if i > 0 {
			body.WriteByte('&')
		}
		body.WriteString(url.QueryEscape(k))
		body.WriteByte('=')
		body.WriteString(url.QueryEscape(params[k]))
	}
	req, err := http.NewRequest("POST", endpointURL, strings.NewReader(body.String()))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	return string(b), nil
}

func uuidV4() string {
	b := make([]byte, 16)
	rand.Read(b)
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// ============================================================================
// Token minting on a blessed Q — port of the gomint BURNIN-proven core.
// The identity fields live in the session file so every token shares ONE
// device identity (exactly like a long-lived browser session).
// ============================================================================

func randToken40() string {
	const charset = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
	b := make([]byte, 40)
	rand.Read(b)
	for i := range b {
		b[i] = charset[int(b[i])%len(charset)]
	}
	return string(b)
}

func randHex32() string {
	b := make([]byte, 16)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func randIntn(n int) int {
	v, err := rand.Int(rand.Reader, big.NewInt(int64(n)))
	if err != nil {
		return n / 2
	}
	return int(v.Int64())
}

// buildWFields — 111-field w-blob, index-positioned (field-by-field ground
// truth in scripts/research/nosdk/gomint.go buildWLog2Fields). Token-A shape:
// w[77]="" (the daemon-harvest shape that PASSes bridge verify).
func buildWFields(convURL, serverIP string, s *BlessSession) [111]string {
	var w [111]string
	w[0] = "W.10054"
	w[5], w[6], w[7] = "Win32", "Chrome", "151.0.0.0"
	ab := make([]byte, 8)
	rand.Read(ab)
	w[20], w[21], w[22] = "17", base64.StdEncoding.EncodeToString(ab), "8"
	w[32], w[34] = randHex32(), "8"
	w[36], w[37] = "Windows", "10"
	w[42] = serverIP
	m11 := 1850 + randIntn(120)
	m20 := m11 + 4 + randIntn(3)
	m23 := m11 + 525 + randIntn(40)
	m30 := m23 + 4 + randIntn(3)
	m40 := m30 + 20 + randIntn(6)
	m90 := m23 + 300 + randIntn(40)
	m91 := m90 + 40 + randIntn(90)
	w[43] = fmt.Sprintf("10-0|11-%d|20-%d|23-%d|30-%d|40-%d|90-%d|91-%d|92-%d",
		m11, m20, m23, m30, m40, m90, m91, m91)
	w[44], w[45] = "true", "true"
	w[47] = "1440*1920"
	w[53] = convURL
	w[63] = "151.0.0.0"
	w[64] = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"
	w[67], w[68] = safApp, "1"
	w[71] = s.SessA
	w[72] = strconv.FormatInt(s.BootTS, 10)
	w[73] = s.SessB
	w[74] = strconv.FormatInt(s.BootTS+int64(m91), 10)
	w[75] = "desktop"
	w[77] = "" // Token-A shape
	w[78] = s.DeviceTag
	w[80] = "5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"
	w[85], w[86] = "0", "0"
	w[87] = strconv.FormatInt(s.QTS, 10)
	w[110] = "[Chromium,Not=A?Brand]"
	return w
}

// deriveWToken — token w = Log2#1 w with "|93-…|94-…" appended to w[43] (the
// ONLY diff; ground truth aes[21]). w[77] stays "".
func deriveWToken(w [111]string) string {
	parts := strings.Split(w[43], "|")
	var m91 int
	for _, p := range parts {
		if strings.HasPrefix(p, "91-") {
			m91, _ = strconv.Atoi(p[3:])
		}
	}
	m93 := m91 + 20 + randIntn(40)
	m94 := m93 + 3 + randIntn(15)
	w[43] = w[43] + "|93-" + strconv.Itoa(m93) + "|94-" + strconv.Itoa(m94)
	return strings.Join(w[:], "#")
}

// mintOnQ assembles a Token-A device token on the blessed Q + sessionKey.
func mintOnQ(s *BlessSession) (string, error) {
	convURL := "https://chat.z.ai/c/" + uuidV4()
	w := buildWFields(convURL, s.IP, s)
	plain := deriveWToken(w)
	wCt, err := aesCBCEncrypt(s.SK, plain)
	if err != nil {
		return "", err
	}
	const tF = "SG_WEB"
	sum := md5.Sum([]byte(tF + "#" + s.Q + "#" + wCt + "#0#daye,raolewoba!"))
	return base64.StdEncoding.EncodeToString([]byte(
		tF + "#" + s.Q + "#" + wCt + "#0#" + hex.EncodeToString(sum[:]))), nil
}

// ============================================================================
// Verify pipeline — byte-exact ports from internal/zbridge/captcha.go
// (generateArg, aliHash, encrypt/rc4) — the path that PASSes live.
// ============================================================================

var argPermTable = [64]int{
	32, 50, 10, 51, 6, 44, 37, 16, 46, 11, 62, 19, 43, 25, 23, 30,
	60, 33, 53, 34, 7, 26, 12, 48, 5, 2, 20, 4, 61, 13, 47, 49,
	18, 29, 27, 22, 1, 17, 39, 56, 41, 38, 55, 31, 15, 58, 52, 40,
	8, 57, 45, 35, 59, 36, 42, 54, 63, 3, 24, 28, 14, 9, 0, 21,
}

const argConstant = "4xrihv8zb8tf1mfj"
const encryptKey = "3e627e1b4c63f913"

func generateArg(certifyID string) string {
	r := argPermTable
	n := argConstant
	rlen := 64
	i, j := 0, 0
	for i < rlen {
		j = (((i + j + r[i] + r[j]) >> 1) + int(n[i%len(n)])) & (rlen - 1)
		if i != j {
			r[i], r[j] = r[j], r[i]
		}
		i++
	}
	t := make([]byte, 0, len(certifyID))
	e, a := 0, 0
	for idx := 0; idx < len(certifyID); idx++ {
		a = ((e ^ a) + (r[e] ^ r[a])) & (rlen - 1)
		if e != a {
			r[e], r[a] = r[a], r[e]
		}
		m := int(certifyID[idx])
		m = m + e + r[e] - a - r[a]
		m = m ^ (r[e] + r[a])
		m = m ^ r[(r[e]+r[a])&(rlen-1)]
		m = m & 255
		t = append(t, byte(m))
		e = (e + 1) & (rlen - 1)
	}
	return base64.StdEncoding.EncodeToString(t)
}

func aliHash(inputStr, saltStr string) string {
	o := inputStr
	r := saltStr
	aLen := len(o)
	m := len(r)
	var e [16]int
	for i := 0; i < 16; i++ {
		e[i] = (i << 4) + (i % 16)
	}
	f := 16
	i, j := 0, 0
	for i < f {
		j = (((i + j + e[i] + e[j]) >> 1) + int(r[i%m])) & (f - 1)
		e[i], e[j] = e[j], e[i]
		i++
	}
	idx, p, q := 0, 0, 0
	for idx < aLen {
		q = ((p ^ q) + (e[p] ^ e[q])) & (f - 1)
		e[p], e[q] = e[q], e[p]
		C := int(o[idx])
		C = (C + p + q) ^ e[p] ^ e[q]
		C = C & 255
		e[p] = C
		p = (p + 1) & (f - 1)
		idx++
	}
	for step := 0; step < 2*f; step++ {
		pos := step % f
		if pos != 0 {
			e[pos] ^= e[pos-1]
		} else {
			e[0] ^= e[f-1]
		}
	}
	const hexLower = "0123456789abcdef"
	var result [32]byte
	for i, b := range e {
		result[i*2] = hexLower[(b>>4)&0xF]
		result[i*2+1] = hexLower[b&0xF]
	}
	return string(result[:])
}

func rc4LikeEncrypt(plaintext []byte) string {
	o := plaintext
	n := encryptKey
	r := argPermTable
	rlen := 64
	oKsa, tKsa := 0, 0
	for oKsa < rlen {
		tKsa = (((oKsa + tKsa + r[oKsa] + r[tKsa]) >> 1) + int(n[oKsa%len(n)])) & (rlen - 1)
		if oKsa != tKsa {
			r[oKsa], r[tKsa] = r[tKsa], r[oKsa]
		}
		oKsa++
	}
	t := make([]byte, 0, len(o))
	e, a := 0, 0
	for nPrga := 0; nPrga < len(o); nPrga++ {
		a = ((e ^ a) + (r[e] ^ r[a])) & (rlen - 1)
		if e != a {
			r[e], r[a] = r[a], r[e]
		}
		m := int(o[nPrga])
		m = m + e + r[e] - a - r[a]
		m = m ^ (r[e] + r[a])
		m = m ^ r[(r[e]+r[a])&(rlen-1)]
		m = m & 255
		t = append(t, byte(m))
		e = (e + 1) & (rlen - 1)
	}
	return base64.StdEncoding.EncodeToString(t)
}

// ============================================================================
// Canary: fresh Init + verify with a freshly minted token (bridge style)
// ============================================================================

func initCaptchaBridge() (string, error) {
	params := map[string]string{
		"AccessKeyId":      bridgeAccessKey,
		"Action":           "InitCaptchaV3",
		"Format":           "JSON",
		"Language":         "en",
		"Mode":             "popup",
		"SceneId":          sceneID,
		"SignatureMethod":  "HMAC-SHA1",
		"SignatureNonce":   uuidV4(),
		"SignatureVersion": "1.0",
		"Timestamp":        time.Now().UTC().Format("2006-01-02T15:04:05Z"),
		"UpLang":           "true",
		"Version":          "2023-03-05",
	}
	params["Signature"] = rpcSign(params, bridgeSecretKey)
	resp, err := rpcPostForm(captchaOpenInitURL, params)
	if err != nil {
		return "", err
	}
	var j struct {
		CertifyID string `json:"CertifyId"`
	}
	if err := json.Unmarshal([]byte(resp), &j); err != nil || j.CertifyID == "" {
		return "", fmt.Errorf("initCaptcha: %s", resp)
	}
	return j.CertifyID, nil
}

// verifyMinted — VerifyCaptchaV3 for one freshly minted token, bridge-style
// (AccessKeyId creds, Track struct field order, aliHash+zlib+b64+rc4).
func verifyMinted(deviceToken, certifyID string) (bool, error) {
	argValue := generateArg(certifyID)
	ct := time.Now().UnixMilli()
	track := fmt.Sprintf(`{"TrackList":{"fi":"","ks":"","mc":"","mp":"","mu":"","startTime":%d,"tc":"","te":"","tmv":""},"TrackStartTime":%d,"VerifyTime":%d,"arg":"%s"}`,
		ct, ct, ct+300, argValue)
	h := aliHash(track, "0000")
	var zb bytes.Buffer
	zw := zlib.NewWriter(&zb)
	zw.Write([]byte(h + track))
	zw.Close()
	fb64 := base64.StdEncoding.EncodeToString(zb.Bytes())
	dataVal := rc4LikeEncrypt([]byte(fb64))

	cvpJSON := fmt.Sprintf(`{"certifyId":"%s","data":"%s","deviceToken":"%s","sceneId":"%s"}`,
		certifyID, dataVal, deviceToken, sceneID)
	params := map[string]string{
		"AccessKeyId":        bridgeAccessKey,
		"Action":             "VerifyCaptchaV3",
		"Format":             "JSON",
		"SignatureMethod":    "HMAC-SHA1",
		"SignatureNonce":     uuidV4(),
		"SignatureVersion":   "1.0",
		"Timestamp":          time.Now().UTC().Format("2006-01-02T15:04:05Z"),
		"Version":            "2023-03-05",
		"SceneId":            sceneID,
		"CertifyId":          certifyID,
		"CaptchaVerifyParam": cvpJSON,
	}
	params["Signature"] = rpcSign(params, bridgeSecretKey)
	resp, err := rpcPostForm(captchaOpenVerifyURL, params)
	if err != nil {
		return false, err
	}
	if os.Getenv("QBLESS_DEBUG") != "" {
		fmt.Println("canary verify resp:", resp)
	}
	return strings.Contains(resp, "\"VerifyResult\":true"), nil
}

// canary mints a token on the session and proves the Q is live.
func canary(s *BlessSession) (bool, error) {
	tok, err := mintOnQ(s)
	if err != nil {
		return false, err
	}
	certifyID, err := initCaptchaBridge()
	if err != nil {
		return false, err
	}
	return verifyMinted(tok, certifyID)
}

// verifyFromEnv — debug probe: VERIFY_TOKEN + VERIFY_CID env vars run the
// exact canary verify path on a known-good (e.g. daemon-minted) token.
func verifyFromEnv() {
	tok := os.Getenv("VERIFY_TOKEN")
	cid := os.Getenv("VERIFY_CID")
	if tok == "" || cid == "" {
		return
	}
	ok, err := verifyMinted(tok, cid)
	fmt.Printf("verifyFromEnv: PASS=%v err=%v\n", ok, err)
	os.Exit(0)
}

// ============================================================================
// TTL probe mode (-probe): monitor a blessed Q's lifetime by minting a fresh
// token on the EXISTING session every -probe-interval and canary-verifying it.
// The first FAIL marks the Q's observed TTL ceiling. No browser involved —
// the session file must already exist (run a normal bless first).
// ============================================================================

func probeTTL(sessionPath string, interval time.Duration, logPath string) {
	raw, err := os.ReadFile(sessionPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "❌ probe: read %s: %v\n", sessionPath, err)
		os.Exit(1)
	}
	var s BlessSession
	if err := json.Unmarshal(raw, &s); err != nil || s.Q == "" {
		fmt.Fprintf(os.Stderr, "❌ probe: bad session %s\n", sessionPath)
		os.Exit(1)
	}

	var logf *os.File
	if logPath != "" {
		logf, err = os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
		if err != nil {
			fmt.Fprintf(os.Stderr, "❌ probe: open log: %v\n", err)
			os.Exit(1)
		}
		defer logf.Close()
	}
	logln := func(format string, args ...interface{}) {
		msg := fmt.Sprintf(format, args...)
		fmt.Println(msg)
		if logf != nil {
			fmt.Fprintf(logf, "%s %s\n", time.Now().Format("2006-01-02 15:04:05"), msg)
		}
	}

	tZero := time.UnixMilli(s.BlessedAt)
	logln("🧪 TTL probe start: Q=%.32s… blessed %s (age %.2fh), interval %s",
		s.Q, tZero.Format("15:04:05"), time.Since(tZero).Hours(), interval)

	// Mint+verify immediately, then loop. On FAIL: log the ceiling, sleep a
	// bit, and keep probing at the same interval — the Q may come back (server
	// side throttling) or be permanently dead; either way the log tells the
	// truth. Stop after 3 consecutive FAILs to avoid burning requests forever.
	fails := 0
	for round := 1; ; round++ {
		age := time.Since(tZero)
		ok, err := canary(&s)
		if err != nil {
			logln("r%03d age=%7.2fh VERIFY-ERROR %v", round, age.Hours(), err)
			fails++
		} else if ok {
			logln("r%03d age=%7.2fh PASS (SecurityToken minted, VerifyResult=true)", round, age.Hours())
			fails = 0
		} else {
			logln("r%03d age=%7.2fh FAIL (VerifyResult=false) — Q dead or untrusted", round, age.Hours())
			fails++
		}
		if fails >= 3 {
			logln("🧪 3 consecutive FAILs at age %.2fh — stopping probe.", time.Since(tZero).Hours())
			os.Exit(3)
		}
		time.Sleep(interval)
	}
}

// probeOnce — single mint+canary cycle, exits 0 on PASS (for cron/audit).
func probeOnce(sessionPath string, quiet bool) {
	raw, err := os.ReadFile(sessionPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "❌ probe: read %s: %v\n", sessionPath, err)
		os.Exit(1)
	}
	var s BlessSession
	if err := json.Unmarshal(raw, &s); err != nil || s.Q == "" {
		fmt.Fprintf(os.Stderr, "❌ probe: bad session %s\n", sessionPath)
		os.Exit(1)
	}
	ok, err := canary(&s)
	if err != nil {
		fmt.Fprintf(os.Stderr, "❌ probe: canary error: %v\n", err)
		os.Exit(1)
	}
	if !ok {
		fmt.Fprintf(os.Stderr, "❌ probe: FAIL at age %.2fh\n", time.Since(time.UnixMilli(s.BlessedAt)).Hours())
		os.Exit(2)
	}
	if !quiet {
		fmt.Printf("✅ probe: PASS at age %.2fh\n", time.Since(time.UnixMilli(s.BlessedAt)).Hours())
	}
	os.Exit(0)
}

// ============================================================================
// Chromium Log1 capture — the ONLY browser step. Stealth JS mirrors
// cmd/token-collector (navigator.webdriver patch etc.) so the page looks
// like the collector's proven-good browser.
// ============================================================================

const stealthJSTemplate = `
Object.defineProperty(navigator, 'webdriver', {get: () => undefined});
window.chrome = window.chrome || {runtime: {}};
Object.defineProperty(navigator, 'languages', {get: () => ['en-US', 'en']});
Object.defineProperty(navigator, 'plugins', {get: () => [1, 2, 3, 4, 5]});
`

// fullChromePath locates the full (non-headless-shell) Chromium binary that
// Playwright installed. QBLESS_CHROME overrides; otherwise per-OS Playwright
// cache globs apply, with real Chrome/Edge installs as a last resort
// (launching headless-shell earns the Q a low trust score — canary F001).
func fullChromePath() string {
	if v := os.Getenv("QBLESS_CHROME"); v != "" {
		return v
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	var patterns []string
	switch runtime.GOOS {
	case "windows":
		patterns = []string{
			filepath.Join(home, "AppData", "Local", "ms-playwright", "chromium-*", "chrome-win", "chrome.exe"),
			filepath.Join(home, "AppData", "Local", "ms-playwright", "chromium-*", "chrome-win64", "chrome.exe"),
			`C:\Program Files\Google\Chrome\Application\chrome.exe`,
			`C:\Program Files (x86)\Google\Chrome\Application\chrome.exe`,
			filepath.Join(home, "AppData", "Local", "Google", "Chrome", "Application", "chrome.exe"),
			`C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`,
			`C:\Program Files\Microsoft\Edge\Application\msedge.exe`,
		}
	case "darwin":
		patterns = []string{
			filepath.Join(home, "Library", "Caches", "ms-playwright", "chromium-*", "chrome-mac", "Chromium.app", "Contents", "MacOS", "Chromium"),
			filepath.Join(home, "Library", "Caches", "ms-playwright", "chromium-*", "chrome-mac-arm64", "Chromium.app", "Contents", "MacOS", "Chromium"),
		}
	default:
		patterns = []string{
			filepath.Join(home, ".cache", "ms-playwright", "chromium-*", "chrome-linux*", "chrome"),
		}
	}
	var matches []string
	for _, pattern := range patterns {
		if found, _ := filepath.Glob(pattern); len(found) > 0 {
			matches = append(matches, found...)
		}
	}
	if len(matches) == 0 {
		return ""
	}
	sort.Strings(matches) // chromium-1234 < chromium-1300 … newest last
	return matches[len(matches)-1]
}

// chromePathOrNil omits ExecutablePath when no full Chrome was found so
// playwright-go falls back to its own default instead of an empty path.
func chromePathOrNil() *string {
	if p := fullChromePath(); p != "" {
		return playwright.String(p)
	}
	return nil
}

func captureLog1DC(wait time.Duration) (dcB64 string, err error) {
	pw, err := playwright.Run()
	if err != nil {
		return "", fmt.Errorf("playwright run: %w", err)
	}
	defer pw.Stop()

	// First run on a fresh machine: the Playwright Chromium build may not
	// exist yet. Install it once (best-effort) so fullChromePath() can find
	// the FULL chrome binary — launching without it would fall back to
	// headless-shell, which earns the Q a low trust score (F001).
	if fullChromePath() == "" {
		fmt.Println("⏳ Playwright Chromium build missing — installing (one-time)…")
		if ierr := playwright.Install(&playwright.RunOptions{Browsers: []string{"chromium"}}); ierr != nil {
			fmt.Fprintf(os.Stderr, "⚠️ playwright install: %v (continuing anyway)\n", ierr)
		}
	}

	browser, err := pw.Chromium.Launch(playwright.BrowserTypeLaunchOptions{
		Headless: playwright.Bool(true),
		// CRITICAL: pin the FULL Chrome binary. Without this Playwright
		// launches chromium_headless_shell — a stripped binary with a DIFFERENT
		// TLS/h2 fingerprint that earns the Q a low trust score at Log1 (canary
		// F001, verified 2026-09-03). netprobe7 (the known-PASS capture) used
		// exactly this executablePath.
		ExecutablePath: chromePathOrNil(),
		Args: []string{
			"--disable-blink-features=AutomationControlled",
			"--no-sandbox",
			"--disable-dev-shm-usage",
			"--disable-gpu",
			"--disable-extensions",
			"--disable-background-networking",
			"--disable-default-apps",
			"--disable-translate",
			"--no-first-run",
			"--disable-renderer-backgrounding",
		},
	})
	if err != nil {
		return "", fmt.Errorf("chromium launch: %w", err)
	}
	defer browser.Close()

	ctx, err := browser.NewContext(playwright.BrowserNewContextOptions{
		UserAgent: playwright.String("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"),
		Locale:    playwright.String("en-US"),
	})
	if err != nil {
		return "", fmt.Errorf("context: %w", err)
	}
	defer ctx.Close()

	page, err := ctx.NewPage()
	if err != nil {
		return "", fmt.Errorf("page: %w", err)
	}
	// NO init script by default. The known-PASS capture (netprobe7) used a
	// vanilla page — no stealth patches. Modifying navigator from an init
	// script CHANGES the page fingerprint relative to plain Chrome/151 and
	// QBLESS_STEALTH=1 opts back in if ever needed.
	if os.Getenv("QBLESS_STEALTH") != "" {
		if err := page.AddInitScript(playwright.Script{Content: playwright.String(stealthJSTemplate)}); err != nil {
			return "", fmt.Errorf("init script: %w", err)
		}
	}

	dcCh := make(chan string, 1)
	failCh := make(chan error, 1)
	page.OnResponse(func(r playwright.Response) {
		if strings.Contains(r.URL(), "cloudauth") {
			if pd, err := r.Request().PostData(); err == nil && strings.Contains(pd, "Action=Log1") {
				go func() {
					defer func() { recover() }()
					body, err := r.Text()
					if err != nil {
						select {
						case failCh <- err:
						default:
						}
						return
					}
					var j struct {
						ResultObject struct {
							DeviceConfig string `json:"DeviceConfig"`
						} `json:"ResultObject"`
					}
					if json.Unmarshal([]byte(body), &j) == nil && j.ResultObject.DeviceConfig != "" {
						select {
						case dcCh <- j.ResultObject.DeviceConfig:
						default:
						}
					}
				}()
			}
		}
	})

	// Block images, fonts, stylesheets, media — only need JS + XHR/fetch
	// to capture the Log1 response. Reduces RAM ~30-50% and speeds up page load.
	if err := page.Route("**/*", func(route playwright.Route) {
		rt := route.Request().ResourceType()
		if rt == "image" || rt == "font" || rt == "stylesheet" || rt == "media" {
			route.Abort()
			return
		}
		route.Continue()
	}); err != nil {
		return "", fmt.Errorf("route: %w", err)
	}

	if _, err := page.Goto("https://chat.z.ai", playwright.PageGotoOptions{
		WaitUntil: playwright.WaitUntilStateDomcontentloaded,
		Timeout:   playwright.Float(45000),
	}); err != nil {
		return "", fmt.Errorf("goto: %w", err)
	}

	// Log1 fires when the z_um SDK activates — on first chat interaction.
	// Click-send like the collector (page load alone is not always enough).
	if tas, err := page.QuerySelectorAll("textarea"); err == nil && len(tas) > 0 {
		_ = tas[0].Fill("hi")
		_ = page.Keyboard().Press("Enter")
	}

	select {
	case dc := <-dcCh:
		// Let the page's z_um SDK finish its post-Log1 handshake (Init,
		// dynamicJS, UploadLog) before the browser dies — the known-PASS
		// capture kept browsing for ~25s. 8s is the new verified-good value
		// (Q is bound at Log1 time, not after handshake); QBLESS_LINGER overrides (ms).
		linger := 8 * time.Second
		if v := os.Getenv("QBLESS_LINGER"); v != "" {
			if ms, err := strconv.Atoi(v); err == nil {
				linger = time.Duration(ms) * time.Millisecond
			}
		}
		time.Sleep(linger)
		return dc, nil
	case err := <-failCh:
		return "", fmt.Errorf("Log1 response read: %w", err)
	case <-time.After(wait):
		return "", fmt.Errorf("timeout waiting for Log1 DeviceConfig (%s)", wait)
	}
}

// ============================================================================
// main
// ============================================================================

func main() {
	out := flag.String("out", "qbless.json", "session file to write")
	verifyFlag := flag.Bool("verify", true, "run canary verify before committing the session")
	timeoutFlag := flag.Int("timeout", 60, "seconds to wait for the Log1 response")
	probeFlag := flag.Bool("probe", false, "TTL probe mode: mint+canary the EXISTING session on an interval (no browser)")
	probeIntFlag := flag.Duration("probe-interval", 20*time.Minute, "-probe: interval between canary rounds")
	probeLogFlag := flag.String("probe-log", "", "-probe: append results to this log file")
	onceFlag := flag.Bool("once", false, "-probe: run a single mint+canary and exit (0=PASS 2=FAIL)")
	quietFlag := flag.Bool("quiet", false, "-probe -once: print only errors")
	flag.Parse()

	verifyFromEnv()

	if *probeFlag {
		if *onceFlag {
			probeOnce(*out, *quietFlag)
		}
		probeTTL(*out, *probeIntFlag, *probeLogFlag)
	}

	start := time.Now()
	fmt.Println("🪄 q-bless: launching Chromium to bless a fresh Q…")

	dcB64, err := captureLog1DC(time.Duration(*timeoutFlag) * time.Second)
	if err != nil {
		fmt.Fprintf(os.Stderr, "❌ %v\n", err)
		os.Exit(1)
	}
	q, sk, ip, qts, err := decryptDeviceConfig(dcB64)
	if err != nil {
		fmt.Fprintf(os.Stderr, "❌ decrypt DeviceConfig: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("✅ Log1 captured in %.1fs: Q=%.40s… sk=%s ip=%s qts=%d\n",
		time.Since(start).Seconds(), q, sk, ip, qts)

	sess := &BlessSession{
		Q:         q,
		SK:        sk,
		QTS:       qts,
		IP:        ip,
		SessA:     randToken40(),
		SessB:     randToken40(),
		BootTS:    qts - 774,
		DeviceTag: randHex32(),
		BlessedAt: time.Now().UnixMilli(),
	}

	if *verifyFlag {
		ok, err := canary(sess)
		if err != nil {
			fmt.Fprintf(os.Stderr, "❌ canary: %v\n", err)
			os.Exit(1)
		}
		if !ok {
			fmt.Fprintln(os.Stderr, "❌ canary verify FAILED (VerifyResult=false) — Q not blessed, refusing to write session")
			os.Exit(2)
		}
		sess.Verified = true
		sess.VerifiedAt = time.Now().UnixMilli()
		if tok, err := mintOnQ(sess); err == nil {
			sess.Canary = tok
		}
		fmt.Println("✅ canary PASS — Q proven blessed (T001)")
	}

	// Atomic write: tmp + rename.
	if err := os.MkdirAll(filepath.Dir(*out), 0o755); err != nil {
		fmt.Fprintf(os.Stderr, "❌ mkdir: %v\n", err)
		os.Exit(1)
	}
	b, _ := json.MarshalIndent(sess, "", "  ")
	tmp := *out + ".tmp"
	if err := os.WriteFile(tmp, b, 0o600); err != nil {
		fmt.Fprintf(os.Stderr, "❌ write: %v\n", err)
		os.Exit(1)
	}
	if err := os.Rename(tmp, *out); err != nil {
		fmt.Fprintf(os.Stderr, "❌ rename: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("✅ session written: %s (total %.1fs, Chromium now dead)\n", *out, time.Since(start).Seconds())
}
