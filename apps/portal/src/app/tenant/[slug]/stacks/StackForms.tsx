"use client";

import { useActionState } from "react";
import { installStack, type FormState } from "./actions";

export type StackFormText = {
  nameLabel: string;
  nameHint: string;
  install: string;
  customLabel: string;
  customSubmit: string;
  problems: string;
  installed: string;
  errors: Record<string, string>;
};

function Result({ state, t }: { state: FormState; t: StackFormText }) {
  if (!state) return null;
  if (state.ok) return <p className="notice">{t.installed}</p>;
  return (
    <div className="notice" role="alert">
      <p>{state.problems ? t.problems : t.errors[state.error] ?? state.error}</p>
      {state.problems && (
        <ul className="small-print">
          {state.problems.map((p) => <li key={p}><code>{p}</code></li>)}
        </ul>
      )}
    </div>
  );
}

function NameField({ t, id, defaultValue }: { t: StackFormText; id: string; defaultValue?: string }) {
  return (
    <>
      <label htmlFor={id}>{t.nameLabel}</label>
      <input className="input" id={id} name="name" required pattern="[a-z][a-z0-9]{1,19}" defaultValue={defaultValue} autoComplete="off" />
      <span className="small-print">{t.nameHint}</span>
    </>
  );
}

export function CatalogInstall({ t, catalogKey, defaultName }: { t: StackFormText; catalogKey: string; defaultName: string }) {
  const [state, action, pending] = useActionState(installStack, null);
  return (
    <form action={action} className="stack">
      <input type="hidden" name="catalogKey" value={catalogKey} />
      <NameField t={t} id={`name-${catalogKey}`} defaultValue={defaultName} />
      <button className="button small" type="submit" disabled={pending}>{t.install}</button>
      <Result state={state} t={t} />
    </form>
  );
}

export function CustomInstall({ t, placeholder }: { t: StackFormText; placeholder: string }) {
  const [state, action, pending] = useActionState(installStack, null);
  return (
    <form action={action} className="stack">
      <NameField t={t} id="name-custom" />
      <label htmlFor="definition">{t.customLabel}</label>
      <textarea className="input mono" id="definition" name="definition" rows={16} required spellCheck={false} placeholder={placeholder} maxLength={65536} />
      <button className="button small" type="submit" disabled={pending}>{t.customSubmit}</button>
      <Result state={state} t={t} />
    </form>
  );
}
