import { useEffect, useMemo, useRef, useState } from "react";

import type { AgentStepDto, ChatTurnDto, LlmStatusDto, TrailStepDto } from "../types";

interface Props {
  turns: ChatTurnDto[];
  busy: boolean;
  error: string | null;
  onSend: (message: string, toAgent: boolean, investigate: boolean) => void;
  // A canvas selection explicitly attached via "Ask about this" in the
  // details panel. Settles which entity the next message is about outright
  // — see chat::EntityContext — and is cleared after one turn rather than
  // sticking silently to every follow-up after it.
  context?: { label: string } | null;
  onClearContext?: () => void;
  // This server was started with `--redact-values`. Directing a question to
  // the agent is still fine (the question is free text the human typed, no
  // different from an ordinary message) — but the server refuses any
  // *reply* to one, since an agent's reply text can't be checked for cell
  // values and this corpus promises none leave the machine. Shown here so a
  // directed question doesn't read as "waiting" when it can, in fact, never
  // be answered.
  redactValues?: boolean;
  // Shown under the title so a reply's provenance is never a guess: which
  // model phrased it, or that none did.
  llm?: LlmStatusDto | null;
  // The investigation in flight, step by step, until its turn lands.
  liveSteps?: AgentStepDto[];
}

// The shared session: a human's message here and an agent's `chat` MCP tool
// call run the same pipeline and land in this same list, tagged by source —
// this is "tell the agent to show me something" and "chat with the
// workbook" as one feature rather than two.
//
// "ask my agent" routes a message past the built-in pipeline entirely (see
// chat::direct_to_agent) and leaves it pending until an attached MCP client
// notices and replies — there is no synchronous hand-off (no MCP client
// implements `sampling/createMessage` today; see project memory
// gui-chat-agent-vs-llm-toggle), so a directed turn can sit open for a
// while, or forever if nothing is attached. That is shown, not hidden.
export function ChatPanel({
  turns,
  busy,
  error,
  onSend,
  context,
  onClearContext,
  redactValues,
  llm,
  liveSteps = [],
}: Props) {
  const [message, setMessage] = useState("");
  const [toAgent, setToAgent] = useState(false);
  const [investigate, setInvestigate] = useState(false);
  const modelOn = !!llm?.settings && llm.settings.privacy !== "off";
  const logRef = useRef<HTMLDivElement>(null);
  // Whether the reader was at (or near) the bottom of the log before the
  // latest change — the only case where a new turn should pull the view
  // down. Someone scrolled up reading an old passage keeps their place when
  // an agent's turn lands; someone who just sent a message sees the answer.
  const stickToBottom = useRef(true);

  useEffect(() => {
    const log = logRef.current;
    if (!log) return;
    const onScroll = () => {
      stickToBottom.current = log.scrollHeight - log.scrollTop - log.clientHeight < 80;
    };
    log.addEventListener("scroll", onScroll);
    return () => log.removeEventListener("scroll", onScroll);
  }, []);

  const lastTurn = turns[turns.length - 1];
  useEffect(() => {
    const log = logRef.current;
    if (!log || !lastTurn) return;
    if (stickToBottom.current || lastTurn.source === "human") {
      log.scrollTop = log.scrollHeight;
      stickToBottom.current = true;
    }
    // A turn's answer can arrive after its id does (a directed turn's reply
    // is a separate turn; the waiting placeholder becomes an answer), so
    // the id and the answer are both triggers.
  }, [lastTurn?.id, lastTurn?.answer, turns.length, liveSteps.length]);

  const submit = () => {
    const text = message.trim();
    if (busy || (!text && !context)) return;
    onSend(text || `what is ${context?.label ?? "this"}?`, toAgent, investigate && !toAgent);
    setMessage("");
  };

  // Which turn ids already have a reply, built once per `turns` change
  // rather than rescanned per rendered turn (an O(n^2) scan on a long
  // session otherwise).
  const repliedTo = useMemo(
    () => new Set(turns.map((t) => t.reply_to).filter((id): id is number => id != null)),
    [turns],
  );

  return (
    <section className="side-section chat-section grow">
      <div className="section-title">Chat</div>
      <div className="chat-model-line">
        {llm?.settings && llm.settings.privacy !== "off"
          ? `${llm.settings.model} · ${llm.settings.privacy}`
          : "no model — replies are the rendered passage"}
      </div>
      <div className="chat-log" ref={logRef}>
        {turns.length === 0 && (
          <div className="empty-note">
            Ask the workbook something, or have an agent call the `chat` MCP
            tool — both show up here, live.
          </div>
        )}
        {turns.map((turn) => {
          const pending = turn.directed_to === "agent" && !repliedTo.has(turn.id);
          return (
            <div
              key={turn.id}
              className={
                "chat-turn source-" + turn.source + (pending ? " pending-agent" : "")
              }
            >
              <div className="chat-turn-message">
                <span className="chat-source-tag">
                  {turn.source === "agent" ? "agent" : "you"}
                </span>
                <ReasonerTag reasoner={turn.reasoner} model={turn.model} />
                {turn.reply_to != null && <span className="chat-reply-tag">reply to</span>}
                {turn.message}
                {turn.directed_to === "agent" && (
                  <span className="chat-directed-tag">→ your agent</span>
                )}
              </div>
              {pending ? (
                <div className="chat-turn-answer chat-turn-waiting">
                  {redactValues
                    ? "an agent's reply is refused while this server runs --redact-values — this will stay pending"
                    : "waiting for an agent to answer…"}
                </div>
              ) : (
                <div className="chat-turn-answer">{turn.answer}</div>
              )}
              {turn.citations.length > 0 && (
                <div className="chat-turn-citations">{turn.citations.join(" · ")}</div>
              )}
              {turn.trail && turn.trail.length > 0 && <Trail steps={turn.trail} />}
            </div>
          );
        })}
        {liveSteps.length > 0 && (
          <div className="chat-turn source-human investigating">
            <div className="chat-turn-message">
              <span className="chat-source-tag">investigating</span>
              {describeLive(liveSteps)}
              <SinceLastStep key={liveSteps.length} />
            </div>
            <LiveSteps steps={liveSteps} />
          </div>
        )}
      </div>
      {context && (
        <div className="context-chip">
          <span>about: {context.label}</span>
          <button className="chip-clear" onClick={onClearContext} aria-label="remove context">
            ×
          </button>
        </div>
      )}
      <label
        className="to-agent-toggle"
        title={
          redactValues
            ? "Route this message to whichever agent is attached over MCP instead of asking the workbook directly. This server was started with --redact-values, so an agent's reply is refused — the question will sit pending."
            : "Route this message to whichever agent is attached over MCP instead of asking the workbook directly. It won't get an instant reply."
        }
      >
        <input
          type="checkbox"
          checked={toAgent}
          onChange={(e) => setToAgent(e.target.checked)}
        />
        ask my agent
      </label>
      <label
        className="to-agent-toggle"
        title={
          modelOn
            ? "Let the chat model drive the workbook tools itself — search, read cells, trace, recompute, what-if — and answer from what they return. Every step shows here and the canvas follows each citation."
            : "Needs a chat model with privacy above `off` — set one in the Chat model panel, or start a bundled model."
        }
      >
        <input
          type="checkbox"
          checked={investigate && !toAgent}
          disabled={!modelOn || toAgent}
          onChange={(e) => setInvestigate(e.target.checked)}
        />
        investigate (the model drives the tools)
      </label>
      {toAgent && redactValues && (
        <div className="to-agent-redact-note">
          this server was started with --redact-values: an agent's reply would be refused, so this
          will stay pending
        </div>
      )}
      <div className="query-row">
        <input
          value={message}
          placeholder={
            toAgent
              ? "ask your attached agent…"
              : investigate
                ? "ask, and watch the model work it out…"
              : context
                ? `ask about ${context.label}…`
                : "ask a follow-up…"
          }
          onChange={(e) => setMessage(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
          spellCheck={false}
        />
        <button className="run primary" onClick={submit} disabled={(!message.trim() && !context) || busy}>
          send
        </button>
      </div>
      {busy && <div className="result-note">{liveSteps.length > 0 ? "investigating…" : "thinking…"}</div>}
      {error && <div className="error-note">{error}</div>}
    </section>
  );
}

// Who reasoned, next to who asked (`chat-source-tag`) — the two axes a
// shared session otherwise makes you reconstruct from the trail and the
// model panel. "none" is left unlabelled; it is the common case and a tag
// on every turn would be noise, not signal.
function ReasonerTag({ reasoner, model }: { reasoner: ChatTurnDto["reasoner"]; model?: string }) {
  switch (reasoner) {
    case "llm":
      return <span className="chat-reasoner-tag reasoner-llm">via {model ?? "model"}</span>;
    case "agent":
      return (
        <span className="chat-reasoner-tag reasoner-agent" title="the GUI's configured model drove the workbook tools itself">
          investigated by {model ?? "model"}
        </span>
      );
    case "external_reply":
      return (
        <span className="chat-reasoner-tag reasoner-external" title="typed by the attached agent's own model, not the GUI's">
          from your agent
        </span>
      );
    default:
      return null;
  }
}

// The tool calls a committed investigation made: name and arguments, with
// the verdict — the tool's own, or the harness's refusal.
function Trail({ steps }: { steps: TrailStepDto[] }) {
  const [open, setOpen] = useState(false);
  const summary = summariseTrail(steps);
  return (
    <div className="chat-trail">
      <button className="link-button" onClick={() => setOpen((o) => !o)}>
        {open ? "▾" : "▸"} {steps.length} tool call{steps.length === 1 ? "" : "s"}: {summary}
      </button>
      {open && (
        <ol className="chat-trail-steps">
          {steps.map((step, i) => (
            <li key={i} className={step.refused ? "refused" : step.ok ? "ok" : "no"}>
              <code>{step.name}</code> {compactArgs(step.args)}
              {step.refused ? " — refused by policy" : step.ok ? "" : " — the tool said no"}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

// The investigation as it happens: each tool call as it is made, each
// result's first lines as it lands, and what the model said between.
function LiveSteps({ steps }: { steps: AgentStepDto[] }) {
  return (
    <ol className="chat-trail-steps live">
      {steps.map((step, i) => {
        switch (step.kind) {
          case "model_call":
            return (
              <li key={i} className="model-call">
                model call #{step.turn}
              </li>
            );
          case "model_reasoning":
            return (
              <li key={i} className="model-reasoning">
                <details>
                  <summary>thought for a while — {step.text.length} chars</summary>
                  <div className="reasoning-text">{step.text}</div>
                </details>
              </li>
            );
          case "model_text":
            return (
              <li key={i} className="model-text">
                {firstLines(step.text, 3)}
              </li>
            );
          case "tool_call":
            return (
              <li key={i} className="tool-call">
                → <code>{step.name}</code> {compactArgs(step.args)}
              </li>
            );
          case "tool_result":
            return (
              <li key={i} className={step.refused ? "refused" : step.ok ? "ok" : "no"}>
                ← <code>{step.name}</code>
                {step.refused ? " refused: " : step.ok ? ": " : " said no: "}
                <span className="tool-result">{firstLines(step.text, 6)}</span>
              </li>
            );
          case "unknown_tool":
            return (
              <li key={i} className="no">
                ✗ no such tool: <code>{step.name}</code>
              </li>
            );
          case "sent_back":
            return (
              <li key={i} className="refused">
                ✗ sent back: {step.reason}
              </li>
            );
        }
      })}
    </ol>
  );
}

function describeLive(steps: AgentStepDto[]): string {
  const calls = steps.filter((s) => s.kind === "tool_call").length;
  const turns = steps.filter((s) => s.kind === "model_call").length;
  return `${turns} model call${turns === 1 ? "" : "s"}, ${calls} tool call${calls === 1 ? "" : "s"} so far`;
}

function summariseTrail(steps: TrailStepDto[]): string {
  const parts: { name: string; n: number }[] = [];
  for (const step of steps) {
    const last = parts[parts.length - 1];
    if (last && last.name === step.name) last.n += 1;
    else parts.push({ name: step.name, n: 1 });
  }
  return parts.map((p) => (p.n > 1 ? `${p.name} ×${p.n}` : p.name)).join(", ");
}

function compactArgs(args: unknown): string {
  if (!args || typeof args !== "object") return "";
  const entries = Object.entries(args as Record<string, unknown>);
  if (entries.length === 0) return "";
  return entries
    .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
    .join(" ");
}

function firstLines(text: string, n: number): string {
  const lines = text.split("\n");
  if (lines.length <= n) return text;
  return lines.slice(0, n).join("\n") + ` … (${lines.length - n} more lines)`;
}

// How long the current step has been running, ticking. A reasoning model
// can think for a minute between one tool result and the next call, and
// a card that does not move for a minute reads as a card that has died.
// Keyed on the step count by the caller, so each step starts its own
// clock.
function SinceLastStep() {
  const [seconds, setSeconds] = useState(0);
  useEffect(() => {
    const started = Date.now();
    const id = setInterval(() => setSeconds(Math.floor((Date.now() - started) / 1000)), 1000);
    return () => clearInterval(id);
  }, []);
  if (seconds < 3) return null;
  return <span className="since-last-step"> · working for {seconds}s</span>;
}
