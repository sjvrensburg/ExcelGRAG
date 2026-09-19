import { useEffect, useMemo, useRef, useState } from "react";

import type { ChatTurnDto } from "../types";

interface Props {
  turns: ChatTurnDto[];
  busy: boolean;
  error: string | null;
  onSend: (message: string, toAgent: boolean) => void;
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
}: Props) {
  const [message, setMessage] = useState("");
  const [toAgent, setToAgent] = useState(false);
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
  }, [lastTurn?.id, lastTurn?.answer, turns.length]);

  const submit = () => {
    const text = message.trim();
    if (busy || (!text && !context)) return;
    onSend(text || `what is ${context?.label ?? "this"}?`, toAgent);
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
            </div>
          );
        })}
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
      {busy && <div className="result-note">thinking…</div>}
      {error && <div className="error-note">{error}</div>}
    </section>
  );
}
