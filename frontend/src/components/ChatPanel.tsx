import { useMemo, useState } from "react";

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
export function ChatPanel({ turns, busy, error, onSend, context, onClearContext }: Props) {
  const [message, setMessage] = useState("");
  const [toAgent, setToAgent] = useState(false);

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
      <div className="chat-log">
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
                {turn.reply_to != null && <span className="chat-reply-tag">reply</span>}
                {turn.message}
                {turn.directed_to === "agent" && (
                  <span className="chat-directed-tag">→ your agent</span>
                )}
              </div>
              {pending ? (
                <div className="chat-turn-answer chat-turn-waiting">
                  waiting for an agent to answer…
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
      <label className="to-agent-toggle" title="Route this message to whichever agent is attached over MCP instead of asking the workbook directly. It won't get an instant reply.">
        <input
          type="checkbox"
          checked={toAgent}
          onChange={(e) => setToAgent(e.target.checked)}
        />
        ask my agent
      </label>
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
