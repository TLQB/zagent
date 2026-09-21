// Code moved from the original main.go monolith during the internal/ restructure.
// See README "Project Structure". Part of the Qwen bridge core (package zbridge).

package zbridge

import (
	"log"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

// ============================================================================
// CONFIGURATION
// ============================================================================

const (
	// sceneID and maxTokenRetries are non-secret constants.
	sceneID         = "didk33e0"
	maxTokenRetries = 5

	// Qwen direct config (non-secret constants). DEFAULT_FE_VERSION is the
	// fallback when the startup scrape fails; initializeSession scrapes the
	// live value on every init, so this only matters for air-gapped starts.
	DEFAULT_FE_VERSION  = "prod-fe-1.1.93"
	qwenLegacyUserAgent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) " + "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
)

// Aliyun captcha credentials and the Qwen signature salt are read from
// environment variables first, falling back to the well-known values that
// shipped with the original monolith (they are already public in git history
// and are shared by every Qwen webchat client; they are not per-user secrets).
// Legacy Z.AI-era signing keys; unused by the Qwen transport (no
// Aliyun signing on chat.qwen.ai) but kept for config-struct compatibility.
// tokenFilePath is the shared token location the wrapper scripts already
// read: ~/.config/qwen-proxy/token (XDG-style, also used on Windows).
func tokenFilePath() string {
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		return filepath.Join(home, ".config", "qwen-proxy", "token")
	}
	return ""
}

// readTokenFile returns the trimmed contents of the shared token file, or ""
// when it is absent/unreadable/empty.
func readTokenFile() string {
	p := tokenFilePath()
	if p == "" {
		return ""
	}
	b, err := os.ReadFile(p)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

var (
	// Placeholders in the public source; real values are injected at link
	// time via -ldflags -X (see scripts/build-zai-proxy-sidecar.sh).
	defaultAliyunAccessKey = "REDACTED000000000000000"
	defaultAliyunSecretKey = "REDACTED000000000000000000000000"

	accessKey = envOr("ALIYUN_ACCESS_KEY", defaultAliyunAccessKey)
	secretKey = envOr("ALIYUN_SECRET_KEY", defaultAliyunSecretKey)
	SALT_KEY  = envOr("QWEN_SALT_KEY", "key-@@@@)))()((9))-xxxx&&&%%%%%")
)

// ---------- Config struct (Qwen) ----------

type Config struct {
	Server struct {
		Port int
		Host string
	}
	Auth struct {
		Enabled bool
		Token   string
	}
	Timeouts struct {
		Default int
	}
	QwenToken string
	AgentMode bool
	// AgentModeVariant selects the agent-mode compatibility shim:
	//   "native" (default) — multi-turn messages upstream, tool exchanges
	//                        defused to text (see agent_native.go)
	//   "modern"           — XML-sectioned prompt shim ported from
	//                        DeepseekFreeAPI (see agent.go)
	//   "legacy"           — the original [ROLE: ...] rewrite shim
	AgentModeVariant string
	Logging          struct {
		Level  string
		Format string
	}
	KnownModels []string
	// StreamHoldback is the number of runes kept pending at the tail of the
	// streamed content before it is forwarded to clients. Qwen's stream is
	// edit-based (edit_content can backtrack and rewrite the tail), and an
	// append-only SSE client cannot take back text it already received.
	// Holding back a small window lets ordinary trailing backtracks be
	// absorbed invisibly. 0 disables the hold-back. See issue #23.
	StreamHoldback int
	// SyncMode disables the async session pool and restores the legacy
	// synchronous flow: every request creates its own chat session first.
	// Used sessions are still deleted on Qwen after each response
	// (throwaway sessions either way — see session_pool.go).
	SyncMode bool
	// SessionPoolSize is the standing batch of pre-made ready chat sessions
	// kept by the async session pool (SESSION_POOL_SIZE, default 5).
	SessionPoolSize int
	// SessionAcquireTimeout bounds, in seconds, how long a request waits for
	// a pooled session before creating one directly instead of stalling
	// (SESSION_ACQUIRE_TIMEOUT, default 10; 0 waits indefinitely).
	SessionAcquireTimeout int
	// StallTimeout is the maximum time in seconds the bridge waits for ANY
	// upstream data from Qwen during streaming before declaring a stall.	// If no SSE chunk arrives within this window the stream is killed and
	// the bridge retries (up to StallMaxRetries times). 0 disables stall
	// detection (legacy behavior — streams can hang forever). Default 120.
	StallTimeout int
	// StallMaxRetries is the maximum number of stall-triggered retries per
	// request. Each retry re-sends the prompt (with the stall nudge appended)
	// to Qwen. Default 2.
	StallMaxRetries int
	// ToolLoopThinking controls thinking (reasoning) on MID-LOOP agent turns —
	// requests whose last incoming message is a tool RESULT (the model must
	// emit the next tool call or the final answer). GLM 5.3 burns 5k–16k
	// reasoning chars (9–49s) on such turns while the decision is usually
	// mechanical (see docs/stall_evident.md); disabling thinking there cuts
	// TTFT and total latency dramatically without hurting the opening turn,
	// which keeps full thinking for task understanding.
	//   "off" (default) — enable_thinking=false on tool-loop turns
	//   "on"            — never throttle (upstream default: thinking on)
	// A client can still force thinking per request via reasoning_effort
	// (upstream contract forces enable_thinking=true when effort is set).
	ToolLoopThinking string
}

func loadConfig() *Config {
	c := &Config{}
	c.Server.Port = 3001
	c.Server.Host = "0.0.0.0"
	c.Auth.Enabled = true
	c.Auth.Token = envOr("AUTH_TOKEN", "Waguri") // documented default; set AUTH_TOKEN in production
	c.Timeouts.Default = 300000
	c.QwenToken = ""
	c.AgentMode = false
	// Native is the default variant (A/B-tested 2026-09-10 on the GLM
	// upstream: ~32-34% lower total latency than modern on the hard-case
	// bench, 0 stalls, and the <glm_block> crash fixed by the tool-wire
	// defuse in agent_native.go). The
	// modern fold shim and the legacy rewrite remain selectable via
	// AGENT_MODE_VARIANT.
	c.AgentModeVariant = "native"
	c.Logging.Level = "info" // was "debug": default-on debug logging wrote every SSE line + full request body to the log (a 121MB log in normal use) and its sync writes sat on the streaming hot path. Set LOG_LEVEL=debug to re-enable.
	c.Logging.Format = "text"
	c.KnownModels = []string{"GLM-5.1", "GLM-5"}
	c.StreamHoldback = 4
	c.SyncMode = false
	c.SessionPoolSize = defaultPoolSize
	c.SessionAcquireTimeout = int(defaultPoolWait / time.Second)
	c.StallTimeout = 120
	c.StallMaxRetries = 2
	c.ToolLoopThinking = "off"

	if p := os.Getenv("PORT"); p != "" {
		if n, err := strconv.Atoi(p); err == nil {
			c.Server.Port = n
		}
	}
	if h := os.Getenv("HOST"); h != "" {
		c.Server.Host = h
	}
	if t := os.Getenv("AUTH_TOKEN"); t != "" {
		c.Auth.Token = t
	}
	if t := os.Getenv("TIMEOUT"); t != "" {
		if n, err := strconv.Atoi(t); err == nil {
			c.Timeouts.Default = n
		}
	}
	if t := os.Getenv("QWEN_TOKEN"); t != "" {
		c.QwenToken = t
	}
	// No QWEN_TOKEN env → fall back to the shared token file. Qwen has no
	// guest flow (every request needs a valid JWT), so the token file is the
	// primary UX: save it once, every later start honors it.
	if c.QwenToken == "" {
		if t := readTokenFile(); t != "" {
			c.QwenToken = t
			log.Printf("[Config] QWEN_TOKEN not set; loaded token from %s", tokenFilePath())
		}
	}
	if am := os.Getenv("AGENT_MODE"); am != "" {
		switch strings.ToLower(am) {
		case "1", "true", "yes", "on", "modern":
			c.AgentMode = true
		case "legacy":
			// Explicit opt-in to the old [ROLE: ...] rewrite shim.
			c.AgentMode = true
			c.AgentModeVariant = "legacy"
		case "0", "false", "no", "off":
			c.AgentMode = false
		}
	}
	// AGENT_MODE_VARIANT overrides the shim variant independently of the
	// AGENT_MODE on/off switch: "native" (default), "modern", or "legacy".
	if v := os.Getenv("AGENT_MODE_VARIANT"); v != "" {
		switch strings.ToLower(v) {
		case "legacy":
			c.AgentModeVariant = "legacy"
		case "modern":
			c.AgentModeVariant = "modern"
		case "native":
			c.AgentModeVariant = "native"
		}
	}
	if l := os.Getenv("LOG_LEVEL"); l != "" {
		c.Logging.Level = l
	}
	if u := os.Getenv("QWEN_BASE_URL"); u != "" {
		// Point the bridge at a mock upstream (CI e2e, local experiments).
		BASE_URL = strings.TrimRight(u, "/")
	}
	if f := os.Getenv("LOG_FORMAT"); f != "" {
		c.Logging.Format = f
	}
	if h := os.Getenv("STREAM_HOLDBACK"); h != "" {
		if n, err := strconv.Atoi(h); err == nil && n >= 0 {
			c.StreamHoldback = n
		}
	}
	// SYNC_MODE restores the legacy synchronous session flow (one chat
	// created per request). Used sessions are still deleted after use.
	if sm := os.Getenv("SYNC_MODE"); sm != "" {
		switch strings.ToLower(sm) {
		case "1", "true", "yes", "on":
			c.SyncMode = true
		case "0", "false", "no", "off":
			c.SyncMode = false
		}
	}
	if ps := os.Getenv("SESSION_POOL_SIZE"); ps != "" {
		if n, err := strconv.Atoi(ps); err == nil && n >= 1 {
			c.SessionPoolSize = n
		}
	}
	if at := os.Getenv("SESSION_ACQUIRE_TIMEOUT"); at != "" {
		if n, err := strconv.Atoi(at); err == nil && n >= 0 {
			c.SessionAcquireTimeout = n
		}
	}
	if st := os.Getenv("STALL_TIMEOUT"); st != "" {
		if n, err := strconv.Atoi(st); err == nil && n >= 0 {
			c.StallTimeout = n
		}
	}
	if sr := os.Getenv("STALL_MAX_RETRIES"); sr != "" {
		if n, err := strconv.Atoi(sr); err == nil && n >= 0 {
			c.StallMaxRetries = n
		}
	}
	if tl := os.Getenv("TOOL_LOOP_THINKING"); tl != "" {
		switch strings.ToLower(tl) {
		case "off", "0", "false", "no":
			c.ToolLoopThinking = "off"
		case "on", "1", "true", "yes":
			c.ToolLoopThinking = "on"
		}
	}
	return c
}

var config = loadConfig()

// agentModern reports whether the modern agent-mode shim (XML-sectioned
// prompt, tolerant marker/payload parsing — see agent.go) is active.
func (c *Config) agentModern() bool {
	return c.AgentMode && !strings.EqualFold(c.AgentModeVariant, "legacy")
}

// agentLegacy reports whether the legacy agent-mode shim ([ROLE: ...]
// message rewriting — see transformMessagesForAgent) is active.
func (c *Config) agentLegacy() bool {
	return c.AgentMode && strings.EqualFold(c.AgentModeVariant, "legacy")
}

// agentNative reports whether the native agent-mode variant is active:
// real multi-turn messages upstream (validated live by the
// EXP_MULTITURN spike — Qwen v2 accepts user/assistant/tool arrays),
// with the tool contract carried in a leading user message and the
// output protocol (<<<TOOL_CALL>>> blocks) unchanged. Takes precedence
// over modern/legacy when set.
func (c *Config) agentNative() bool {
	return c.AgentMode && strings.EqualFold(c.AgentModeVariant, "native")
}

// throttleThinkingForTurn applies the TOOL_LOOP_THINKING policy to one
// request. GLM 5.3 burns 5k–16k reasoning chars (9–49s per turn) on
// mid-loop agent turns whose decision is usually mechanical — the next tool
// call or the final answer over results already sitting in the prompt
// (docs/stall_evident.md). When the incoming conversation ends with a tool
// RESULT, thinking is disabled (enable_thinking=false upstream) unless the
// operator opted out with TOOL_LOOP_THINKING=on. The opening turn (user
// message last) keeps full thinking for task understanding, and a client
// forcing reasoning_effort keeps its contract (effort forces thinking on —
// enforced later in sendToQwen).
//
// lastIsToolResult must be computed from the ORIGINAL (pre-agent-transform)
// messages — the modern shim folds the conversation into a single user
// message, which would mask the tool-result tail.
func (c *Config) throttleThinkingForTurn(opts *QwenSendOptions, lastIsToolResult bool) {
	if c.ToolLoopThinking != "off" || !lastIsToolResult {
		return
	}
	if opts.Thinking == nil {
		f := false
		opts.Thinking = &f
		logInfo("[tool-loop-throttle] last message is a tool result — enable_thinking=false for this turn (TOOL_LOOP_THINKING=off)")
	}
}

// lastMessageIsToolResult reports whether the conversation's LAST message is
// a tool result (the tool-loop continuation case, case-insensitive role).
func lastMessageIsToolResult(messages []Message) bool {
	if len(messages) == 0 {
		return false
	}
	return strings.EqualFold(strings.TrimSpace(messages[len(messages)-1].Role), "tool")
}
