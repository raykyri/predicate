import type { AcpAuthPrompt } from "../types";

interface AcpAuthCardProps {
  prompt: AcpAuthPrompt;
  onSelectMethod: (methodId: string, index: number) => void;
}

/**
 * Desktop first-run sign-in card for ACP agents. Choices are answered by
 * writing into the pane (the bridge always prompts there too), so remote and
 * headless sessions keep working without this surface.
 */
export default function AcpAuthCard({ prompt, onSelectMethod }: AcpAuthCardProps) {
  return (
    <div className="acp-auth-card" role="region" aria-label="Sign in required">
      <div className="acp-auth-card-header">
        <strong>Sign in required</strong>
        <span className="acp-auth-card-hint">
          Pick a method, or type the number in the terminal pane.
        </span>
      </div>
      {prompt.error ? (
        <p className="acp-auth-card-error" role="alert">
          {prompt.error}
        </p>
      ) : null}
      <ul className="acp-auth-card-methods">
        {prompt.methods.map((method, index) => (
          <li key={method.id}>
            <button
              type="button"
              className="control-button acp-auth-card-method"
              onClick={() => onSelectMethod(method.id, index + 1)}
            >
              <span className="acp-auth-card-index">{index + 1}</span>
              <span className="acp-auth-card-method-body">
                <span className="acp-auth-card-method-name">{method.name}</span>
                {method.description ? (
                  <span className="acp-auth-card-method-description">{method.description}</span>
                ) : null}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
