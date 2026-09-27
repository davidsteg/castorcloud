import type { Dictionary } from "@/i18n";
import { AI_PROVIDER } from "@/lib/config";

// How the assistant works. The provider statement depends on the configured
// AI provider: the "swiss" promise is only shown when it is actually true.
export function AssistantInfo({ t }: { t: Dictionary }) {
  const a = t.assistantInfo;
  return (
    <div className="assistant-info">
      <ol className="flow-steps">
        {a.steps.map((step, i) => (
          <li key={step.title}>
            <span className="num">{i + 1}</span>
            <h3>{step.title}</h3>
            <p>{step.text}</p>
          </li>
        ))}
      </ol>

      <div className="assistant-grid">
        <div className="panel">
          <h3>{a.guaranteesTitle}</h3>
          <ul className="guarantees">
            {a.guarantees.map((g) => <li key={g}>{g}</li>)}
          </ul>
          <p className="notice" style={{ marginTop: 18 }}>{a.provider[AI_PROVIDER]}</p>
        </div>
        <div className="panel">
          <h3>{a.channelsTitle}</h3>
          <ul className="channels">
            {a.channels.map((c) => (
              <li key={c.name}>
                <b>{c.name}</b>
                <span>{c.text}</span>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </div>
  );
}
