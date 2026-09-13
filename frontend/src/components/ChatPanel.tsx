import { useState } from "react";

import type { ChatTurnDto } from "../types";

interface Props {
  turns: ChatTurnDto[];
  busy: boolean;
  error: string | null;
  onSend: (message: string) => void;
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
export function ChatPanel({ turns, busy, error, onSend, context, onClearContext }: Props) {
  const [message, setMessage] = useState("");

  const submit = () => {
    const text = message.trim();
    if (busy || (!text && !context)) return;
    onSend(text || `what is ${context?.label ?? "this"}?`);
    setMessage("");
  };

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
        {turns.map((turn) => (
          <div key={turn.id} className={"chat-turn source-" + turn.source}>
            <div className="chat-turn-message">
              <span className="chat-source-tag">{turn.source === "agent" ? "agent" : "you"}</span>
              {turn.message}
            </div>
            <div className="chat-turn-answer">{turn.answer}</div>
            {turn.citations.length > 0 && (
              <div className="chat-turn-citations">{turn.citations.join(" · ")}</div>
            )}
          </div>
        ))}
      </div>
      {context && (
        <div className="context-chip">
          <span>about: {context.label}</span>
          <button className="chip-clear" onClick={onClearContext} aria-label="remove context">
            ×
          </button>
        </div>
      )}
      <div className="query-row">
        <input
          value={message}
          placeholder={context ? `ask about ${context.label}…` : "ask a follow-up…"}
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
