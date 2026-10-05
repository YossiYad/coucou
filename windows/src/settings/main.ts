// Settings window — the place where anything that writes to disk is confirmed.
// Stage 2 covers the Claude Code hooks and the general preferences; API keys and
// integrations land here too in a later stage.

import "./settings.css";
import { Bridge, onEvent, type HookStatus, type ModelInfo } from "../core/bridge";
import { DEFAULT_SETTINGS, PERMISSION_MODES, type PermissionMode, type Provider, type Settings } from "../core/state";
import { h, clear } from "../views/dom";

let settings: Settings = { ...DEFAULT_SETTINGS };
/** Puts the permission mode chosen in the island into the select. */
let syncMode = () => {};
/** Keeps the start option in step with the island. */
let syncPlace = () => {};
let version = "";
/** Where API keys live on this platform, for the wording only. */
let keyStore = "the Windows Credential Manager";

/** `coucou-hook.exe` on Windows, `coucou-hook` on Linux — whatever the path ends in. */
function hookFileName(path: string): string {
  return path.split(/[\\/]/).pop() || "coucou-hook";
}

const root = document.getElementById("settings-root")!;

async function save() {
  await Bridge.saveSettings(settings);
}

// ── Reusable bits ─────────────────────────────────────────────────────────────

function toggle(on: boolean, onChange: (v: boolean) => void): HTMLElement {
  const el = h("button", { class: on ? "switch on" : "switch", "aria-pressed": on });
  el.addEventListener("click", () => {
    const next = !el.classList.contains("on");
    el.classList.toggle("on", next);
    onChange(next);
  });
  return el;
}

function statusDot(ok: boolean): HTMLElement {
  return h("i", { class: "dot", style: `background:${ok ? "#22c55e" : "#f4505e"}` });
}

function renderDiff(text: string): HTMLElement {
  const box = h("div", { class: "diff" });
  for (const line of text.split("\n")) {
    const cls = line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : "ctx";
    box.append(h("div", { class: cls, text: line }));
  }
  return box;
}

// ── Claude Code section ───────────────────────────────────────────────────────

function claudeSection(status: HookStatus): HTMLElement {
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  const section = h(
    "section",
    {},
    h("h2", {}, statusDot(status.installed), h("span", { text: "Claude Code" })),
    body,
  );

  const rebuild = async () => {
    const fresh = await Bridge.hooksStatus();
    if (fresh) Object.assign(status, fresh);
    clear(body);
    draw();
    const head = section.querySelector("h2")!;
    clear(head);
    head.append(statusDot(status.installed), h("span", { text: "Claude Code" }));
  };

  function draw() {
    body.append(
      h("div", {
        class: "hint",
        text: status.installed
          ? "Coucou is hooked into your Claude Code sessions. Tool calls, questions and permission requests show up in the island, and you can answer them there."
          : "Install the hooks to see your Claude Code sessions in the island and approve permissions without leaving what you are doing.",
      }),
      h("div", { class: "row" },
        h("label", { text: "settings.json" }),
        h("span", { class: "path", text: status.settingsPath }),
      ),
      h("div", { class: "row" },
        h("label", { text: "Relay" }),
        h("span", { class: "path", text: status.hookPath }),
        statusDot(status.hookReady),
      ),
    );

    if (!status.hookReady) {
      body.append(h("div", {
        class: "notice warn",
        text: `${hookFileName(status.hookPath)} is not in place yet. Restart Coucou; if it still fails, build it with \`cargo build -p coucou-hook\`.`,
      }));
    }

    const actions = h("div", { class: "row" });
    const install = h("button", {
      class: "primary",
      text: status.installed ? "Reinstall hooks…" : "Install hooks…",
      onclick: () => showPreview(true),
    });
    // Writing hook commands that point at a relay which isn't there would give
    // every Claude Code session a broken hook and nothing to show for it.
    if (!status.hookReady) {
      install.disabled = true;
      install.title = "The relay isn't installed yet.";
    }
    actions.append(install);
    if (status.installed) {
      actions.append(h("button", {
        class: "danger",
        text: "Uninstall hooks…",
        onclick: () => showPreview(false),
      }));
    }
    body.append(actions);
  }

  async function showPreview(install: boolean) {
    let preview;
    try {
      preview = await Bridge.hooksPreview(install);
    } catch (err) {
      // An unreadable or invalid settings.json stops here rather than being
      // treated as empty and written over.
      clear(body);
      body.append(
        h("div", { class: "notice err", text: String(err).replace(/^Error:\s*/, "") }),
        h("div", { class: "row" }, h("button", {
          text: "Back",
          onclick: () => { clear(body); draw(); },
        })),
      );
      return;
    }
    if (!preview) return;
    clear(body);
    body.append(
      h("div", {
        class: "hint",
        text: install
          ? "This is exactly what will change in your settings.json. Your own hooks are left untouched."
          : "This removes Coucou's entries only. Your own hooks are left untouched.",
      }),
      renderDiff(preview.diff),
      h("div", { class: "row" },
        h("span", { class: "path", text: `Backup → ${preview.backup}` }),
      ),
    );
    const confirm = h("button", {
      class: install ? "primary" : "danger",
      text: install ? "Back up and write" : "Back up and remove",
    });
    confirm.addEventListener("click", async () => {
      confirm.disabled = true;
      try {
        const backup = await Bridge.hooksApply(install, preview.fingerprint);
        clear(body);
        body.append(h("div", {
          class: "notice ok",
          text: `Done. Previous settings saved as ${backup}. Open a new Claude Code session to pick the hooks up.`,
        }));
        window.setTimeout(() => void rebuild(), 2600);
      } catch (err) {
        confirm.disabled = false;
        body.append(h("div", { class: "notice err", text: `Could not write: ${String(err)}` }));
      }
    });
    body.append(h("div", { class: "row" }, confirm, h("button", {
      text: "Cancel",
      onclick: () => { clear(body); draw(); },
    })));
  }

  draw();
  return section;
}

// ── AI chat section ───────────────────────────────────────────────────────────

interface ProviderDef {
  id: Provider;
  name: string;
  /** Key-store entry, or null for the local server, which needs none. */
  key: string | null;
  placeholder: string;
  hint: string;
}

const PROVIDERS: ProviderDef[] = [
  {
    id: "anthropic", name: "Claude (Anthropic)", key: "anthropic-api-key",
    placeholder: "sk-ant-...", hint: "Searches the web, reads PDFs and images.",
  },
  {
    id: "openai", name: "ChatGPT (OpenAI)", key: "openai-api-key",
    placeholder: "sk-...", hint: "Searches the web, reads PDFs and images.",
  },
  {
    id: "gemini", name: "Gemini (Google)", key: "gemini-api-key",
    placeholder: "AIza...", hint: "Searches with Google, reads PDFs and images.",
  },
  {
    id: "local", name: "Local model (Ollama, LM Studio...)", key: null, placeholder: "",
    hint: "Runs on this computer, nothing leaves it. No web search or PDFs; images need a vision model.",
  },
];

/** Offered for Claude before a key is saved, when the live list can't be asked for. */
const CLAUDE_MODELS: ModelInfo[] = [
  { id: "claude-opus-5", label: "Claude Opus 5" },
  { id: "claude-sonnet-5", label: "Claude Sonnet 5" },
  { id: "claude-haiku-4-5", label: "Claude Haiku 4.5" },
];

const OTHER_MODEL = "__other__";

function modelOf(p: Provider): string {
  switch (p) {
    case "openai": return settings.openaiModel;
    case "gemini": return settings.geminiModel;
    case "local": return settings.localModel;
    default: return settings.model;
  }
}

function setModelOf(p: Provider, id: string) {
  switch (p) {
    case "openai": settings.openaiModel = id; break;
    case "gemini": settings.geminiModel = id; break;
    case "local": settings.localModel = id; break;
    default: settings.model = id;
  }
}

function aiSection(): HTMLElement {
  const dot = statusDot(false);
  const state = h("span", { class: "hint" });
  const feedback = h("div", {});
  const grow = "flex:1 1 auto;min-width:0";

  const provider = h("select", {}) as HTMLSelectElement;
  for (const p of PROVIDERS) provider.append(h("option", { value: p.id, text: p.name }));
  provider.value = settings.provider;

  const keyField = h("input", {
    type: "password", style: grow, autocomplete: "off", spellcheck: "false",
  }) as HTMLInputElement;
  const saveKey = h("button", { class: "primary", text: "Save key" });
  const clearKey = h("button", { class: "danger", text: "Remove" });
  const keyRow = h("div", { class: "row" }, h("label", { text: "API key" }), keyField, saveKey, clearKey);

  // Keys from the user's other accounts with the same provider, stored as
  // <key>-2 ... <key>-5 and tried in turn when one runs out.
  const extraField = h("input", {
    type: "password", style: grow, autocomplete: "off", spellcheck: "false",
  }) as HTMLInputElement;
  const addExtra = h("button", { text: "Add key" });
  const clearExtra = h("button", { class: "danger", text: "Remove them" });
  const extraRow = h(
    "div",
    {},
    h("div", { class: "row" }, h("label", { text: "Other accounts" }), extraField, addExtra, clearExtra),
    h("div", {
      class: "hint",
      text: "Keys from your other accounts with the same provider, up to four. When one account's quota runs out, the next one answers.",
    }),
  );
  const extraSlots = [2, 3, 4, 5];
  const extraName = (base: string, slot: number) => `${base}-${slot}`;
  async function storedExtras(base: string): Promise<number[]> {
    const found: number[] = [];
    for (const slot of extraSlots) if ((await Bridge.secretPresent(extraName(base, slot))) ?? false) found.push(slot);
    return found;
  }
  async function showExtras() {
    const p = current();
    if (!p.key) return;
    const found = await storedExtras(p.key);
    extraField.placeholder = found.length
      ? `${found.length} more key${found.length > 1 ? "s" : ""} stored; paste another to add it`
      : "Paste a key from another account";
    clearExtra.style.display = found.length ? "" : "none";
    addExtra.toggleAttribute("disabled", found.length >= extraSlots.length);
  }
  addExtra.addEventListener("click", async () => {
    const p = current();
    const value = extraField.value.trim();
    if (!value || !p.key) return;
    const found = await storedExtras(p.key);
    const free = extraSlots.find((s) => !found.includes(s));
    if (free == null) return;
    try {
      await Bridge.secretSet(extraName(p.key, free), value);
      extraField.value = "";
      note("ok", "Added. It never touches disk.");
      await showExtras();
    } catch (err) {
      note("err", `Could not save: ${String(err)}`);
    }
  });
  clearExtra.addEventListener("click", async () => {
    const p = current();
    if (!p.key) return;
    try {
      for (const slot of extraSlots) await Bridge.secretClear(extraName(p.key, slot));
      note("ok", "The other accounts' keys are removed.");
      await showExtras();
    } catch (err) {
      note("err", `Could not remove: ${String(err)}`);
    }
  });

  const screenScope = h("select", {}) as HTMLSelectElement;
  screenScope.append(
    h("option", { value: "mouse", text: "The screen the mouse is on" }),
    h("option", { value: "all", text: "All screens" }),
  );
  screenScope.value = settings.screenScope;
  screenScope.addEventListener("change", () => {
    settings.screenScope = screenScope.value as Settings["screenScope"];
    void save();
  });
  const screenRow = h(
    "div",
    {},
    h("div", { class: "row" }, h("label", { text: "Screen to share" }), screenScope),
    h("div", {
      class: "hint",
      text: "The screen button in the chat sends a screenshot with each question, so it can guide you step by step. When it is off and you ask for help with something on the screen, it asks first and shows you the screenshot before anything is sent.",
    }),
  );

  const speakToggle = toggle(settings.speakAnswers, (on) => {
    settings.speakAnswers = on;
    void save();
  });
  const mic = h("select", { style: grow }) as HTMLSelectElement;
  mic.append(h("option", { value: "", text: "System default" }));
  void Bridge.listMicrophones().then((list) => {
    for (const m of list ?? []) mic.append(h("option", { value: m.id, text: m.label }));
    mic.value = settings.microphone;
  });
  mic.addEventListener("change", () => {
    settings.microphone = mic.value;
    void save();
  });
  const voiceRow = h(
    "div",
    {},
    h("div", { class: "row" }, h("label", { text: "Microphone" }), mic),
    h("div", { class: "row" }, h("label", { text: "Speak answers" }), speakToggle),
    h("div", {
      class: "hint",
      text: "Talk to it with the microphone button in the chat: click, speak, click again. Your words become text through Gemini (it needs a Gemini key). When this is on, the answer to a spoken question is read out loud.",
    }),
  );

  const fallbackToggle = toggle(settings.aiFallback, (on) => {
    settings.aiFallback = on;
    void save();
  });
  const fallbackRow = h(
    "div",
    {},
    h("div", { class: "row" }, h("label", { text: "If it can't answer" }), fallbackToggle),
    h("div", {
      class: "hint",
      text: "When the chosen model can't answer (quota used up, overloaded, offline, no key), ask the next one: the same model on your other accounts, then a lighter Gemini model, then the other providers you have keys for, and the local model last.",
    }),
  );

  const urlField = h("input", {
    type: "text", style: grow, spellcheck: "false", placeholder: "http://localhost:11434",
  }) as HTMLInputElement;
  const connect = h("button", { class: "primary", text: "Connect" });
  const urlRow = h("div", { class: "row" }, h("label", { text: "Server" }), urlField, connect);

  const startField = h("input", {
    type: "text", style: grow, spellcheck: "false", placeholder: "podman start ollama",
  }) as HTMLInputElement;
  const stopField = h("input", {
    type: "text", style: grow, spellcheck: "false", placeholder: "podman stop ollama",
  }) as HTMLInputElement;
  const lifecycle = h(
    "div",
    {},
    h("div", { class: "row" }, h("label", { text: "Start with" }), startField),
    h("div", { class: "row" }, h("label", { text: "Stop with" }), stopField),
    h("div", {
      class: "hint",
      text: "Optional. Coucou starts the server when a question finds it down, and stops it 10 minutes after the last one, so it takes no memory between uses. A server you started yourself is left alone.",
    }),
  );
  const toolsToggle = toggle(settings.toolsEnabled, (on) => {
    settings.toolsEnabled = on;
    void save();
  });
  const modeSelect = h("select", {}) as HTMLSelectElement;
  for (const m of PERMISSION_MODES) modeSelect.append(h("option", { value: m.id, text: m.label }));
  modeSelect.value = settings.permissionMode;
  modeSelect.addEventListener("change", () => {
    settings.permissionMode = modeSelect.value as PermissionMode;
    void save();
  });
  syncMode = () => {
    modeSelect.value = settings.permissionMode;
  };
  const acting = h(
    "div",
    {},
    h("div", { class: "row" }, h("label", { text: "Let it act" }), toolsToggle),
    h("div", { class: "row" }, h("label", { text: "Changes" }), modeSelect),
    h("div", {
      class: "hint",
      text: "Find, read and change files in your home folder (text, spreadsheets and Word documents, in place), read web pages and search the web. Hidden files stay off limits. Every change shows as a diff first; Manual waits for Allow, Auto asks only before what can't be undone (deleting, force-pushing, closing apps, shutting down), Accept edits never asks about files (a backup is kept), Plan changes nothing and proposes a plan. The mode can also be switched from the chat. Local models need tool support, such as qwen3.",
    }),
  );

  startField.addEventListener("change", () => {
    settings.localStartCommand = startField.value.trim();
    void save();
  });
  stopField.addEventListener("change", () => {
    settings.localStopCommand = stopField.value.trim();
    void save();
  });

  const model = h("select", { style: grow }) as HTMLSelectElement;
  const refresh = h("button", { text: "Refresh" });
  const custom = h("input", {
    type: "text", style: grow, spellcheck: "false", placeholder: "Model name, as the provider writes it",
  }) as HTMLInputElement;
  const useCustom = h("button", { text: "Use" });
  const customRow = h("div", { class: "row" }, h("label", { text: "" }), custom, useCustom);

  const onHide = h("select", {}) as HTMLSelectElement;
  onHide.append(
    h("option", { value: "keep", text: "Keep the conversation" }),
    h("option", { value: "clear", text: "Start a new one" }),
  );
  onHide.value = settings.clearChatOnHide ? "clear" : "keep";
  onHide.addEventListener("change", () => {
    settings.clearChatOnHide = onHide.value === "clear";
    void save();
  });

  const current = () => PROVIDERS.find((p) => p.id === provider.value) ?? PROVIDERS[0];

  function note(kind: "ok" | "err", text: string) {
    clear(feedback);
    feedback.append(h("div", { class: `notice ${kind}`, text }));
  }

  function fillModels(list: ModelInfo[]) {
    const p = current().id;
    if (!modelOf(p) && list.length) {
      setModelOf(p, list[0].id);
      void save();
    }
    const chosen = modelOf(p);
    clear(model);
    for (const m of list) {
      model.append(h("option", { value: m.id, text: m.label === m.id ? m.id : `${m.label}  (${m.id})` }));
    }
    if (chosen && !list.some((m) => m.id === chosen)) model.append(h("option", { value: chosen, text: chosen }));
    model.append(h("option", { value: OTHER_MODEL, text: "Other…" }));
    model.value = chosen || OTHER_MODEL;
    customRow.style.display = model.value === OTHER_MODEL ? "" : "none";
  }

  /** `start`: an explicit Refresh or Connect may wake a local server that is off. */
  async function loadModels(start = false) {
    const p = current();
    if (p.key) {
      const present = (await Bridge.secretPresent(p.key)) ?? false;
      clearKey.style.display = present ? "" : "none";
      keyField.placeholder = present ? "••••••••••••  (stored)" : p.placeholder;
      if (!present) {
        dot.style.background = "#f4505e";
        state.textContent = `No key yet, the chat needs one. ${p.hint}`;
        fillModels(p.id === "anthropic" ? CLAUDE_MODELS : []);
        return;
      }
      state.textContent = `Key saved in ${keyStore}. ${p.hint}`;
    } else {
      state.textContent = p.hint;
    }
    try {
      const list = await Bridge.aiModels(p.id, start);
      if (current() !== p) return; // the provider changed while this loaded
      fillModels(list);
      dot.style.background = list.length ? "#22c55e" : "#f4505e";
      if (!list.length) {
        note("err", p.key
          ? "No chat models found for this key."
          : "The server answered but has no models yet. With Ollama: ollama pull <model>.");
      }
    } catch (err) {
      if (current() !== p) return;
      const message = String(err).replace(/^Error:\s*/, "");
      fillModels(p.id === "anthropic" ? CLAUDE_MODELS : []);
      // Off on purpose, between uses: not an error.
      const off = message.startsWith("The local server is off");
      dot.style.background = off ? "#9ca3af" : "#f4505e";
      note(off ? "ok" : "err", message);
    }
  }

  function showProvider() {
    const p = current();
    keyRow.style.display = p.key ? "" : "none";
    extraRow.style.display = p.key ? "" : "none";
    void showExtras();
    urlRow.style.display = p.key ? "none" : "";
    lifecycle.style.display = p.key ? "none" : "";
    urlField.value = settings.localUrl;
    startField.value = settings.localStartCommand;
    stopField.value = settings.localStopCommand;
    keyField.value = "";
    clear(feedback);
    void loadModels();
  }

  provider.addEventListener("change", () => {
    settings.provider = provider.value as Provider;
    void save();
    showProvider();
  });

  saveKey.addEventListener("click", async () => {
    const p = current();
    const value = keyField.value.trim();
    if (!value || !p.key) return;
    try {
      await Bridge.secretSet(p.key, value);
      keyField.value = "";
      note("ok", "Saved. It never touches disk.");
      await loadModels();
    } catch (err) {
      note("err", `Could not save: ${String(err)}`);
    }
  });

  clearKey.addEventListener("click", async () => {
    const p = current();
    if (!p.key) return;
    try {
      await Bridge.secretClear(p.key);
      note("ok", "Key removed.");
      await loadModels();
    } catch (err) {
      note("err", `Could not remove: ${String(err)}`);
    }
  });

  connect.addEventListener("click", async () => {
    const value = urlField.value.trim();
    if (!value) return;
    settings.localUrl = value;
    await save();
    clear(feedback);
    await loadModels(true);
  });

  refresh.addEventListener("click", () => {
    clear(feedback);
    void loadModels(true);
  });

  model.addEventListener("change", () => {
    customRow.style.display = model.value === OTHER_MODEL ? "" : "none";
    if (model.value === OTHER_MODEL) {
      custom.focus();
      return;
    }
    setModelOf(current().id, model.value);
    void save();
  });

  useCustom.addEventListener("click", () => {
    const id = custom.value.trim();
    if (!id) return;
    setModelOf(current().id, id);
    void save();
    custom.value = "";
    void loadModels();
  });

  showProvider();

  return h(
    "section",
    {},
    h("h2", {}, dot, h("span", { text: "AI chat" })),
    state,
    h("div", { class: "row" }, h("label", { text: "Provider" }), provider),
    keyRow,
    extraRow,
    urlRow,
    h("div", { class: "row" }, h("label", { text: "Model" }), model, refresh),
    lifecycle,
    customRow,
    h("div", { class: "row" }, h("label", { text: "When it hides" }), onHide),
    fallbackRow,
    voiceRow,
    screenRow,
    acting,
    feedback,
  );
}

// ── Integrations section ──────────────────────────────────────────────────────

interface IntegrationDef {
  id: string;
  name: string;
  color: string;
  /** Key-store keys, in the order they are shown. */
  fields: { key: string; label: string; placeholder: string; secret: boolean }[];
}

const INTEGRATIONS: IntegrationDef[] = [
  { id: "integration_stripe", name: "Stripe", color: "#0570DE",
    fields: [{ key: "stripe-api-key", label: "Secret key", placeholder: "sk_live_…", secret: true }] },
  { id: "integration_github", name: "GitHub", color: "#F4505E",
    fields: [{ key: "github-token", label: "Token", placeholder: "ghp_…", secret: true }] },
  { id: "integration_vercel", name: "Vercel", color: "#7C5CFF",
    fields: [{ key: "vercel-token", label: "Token", placeholder: "…", secret: true }] },
  { id: "integration_n8n", name: "n8n", color: "#F29B38",
    fields: [
      { key: "n8n-url", label: "Instance URL", placeholder: "https://n8n.example.com", secret: false },
      { key: "n8n-api-key", label: "API key", placeholder: "…", secret: true },
    ] },
  { id: "integration_resend", name: "Resend", color: "#22C55E",
    fields: [{ key: "resend-api-key", label: "API key", placeholder: "re_…", secret: true }] },
  { id: "integration_notion", name: "Notion", color: "#8C8C8C",
    fields: [{ key: "notion-api-key", label: "Integration token", placeholder: "ntn_…", secret: true }] },
  { id: "integration_calcom", name: "Cal.com", color: "#C9956A",
    fields: [{ key: "calcom-api-key", label: "API key", placeholder: "cal_…", secret: true }] },
];

const MAX_ACTIVE = 4;

function integrationsSection(present: Record<string, boolean>): HTMLElement {
  const note = h("div", { class: "hint" });
  const list = h("div", { style: "display:flex;flex-direction:column;gap:14px" });

  function updateNote() {
    const used = settings.activeIntegrations.length;
    note.textContent = `Pick up to ${MAX_ACTIVE} pills to show next to Mochi — ${used}/${MAX_ACTIVE} in use. Keys are stored in ${keyStore}, never on disk.`;
  }

  for (const def of INTEGRATIONS) {
    const active = settings.activeIntegrations.includes(def.id);
    const sw = h("button", { class: active ? "switch on" : "switch" });
    sw.addEventListener("click", () => {
      const on = settings.activeIntegrations.includes(def.id);
      if (on) {
        settings.activeIntegrations = settings.activeIntegrations.filter((x) => x !== def.id);
      } else {
        if (settings.activeIntegrations.length >= MAX_ACTIVE) return;
        settings.activeIntegrations = [...settings.activeIntegrations, def.id];
      }
      sw.classList.toggle("on", !on);
      updateNote();
      void save();
    });

    const rows = h("div", { style: "display:flex;flex-direction:column;gap:6px;flex:1 1 auto;min-width:0" });
    for (const field of def.fields) {
      const input = h("input", {
        type: field.secret ? "password" : "text",
        placeholder: present[field.key] ? "••••••••  (stored)" : field.placeholder,
        autocomplete: "off",
        spellcheck: "false",
        style: "flex:1 1 auto;min-width:0",
      }) as HTMLInputElement;
      const saveBtn = h("button", { text: "Save" });
      const dotEl = statusDot(present[field.key] ?? false);
      saveBtn.addEventListener("click", async () => {
        const value = input.value.trim();
        try {
          await Bridge.secretSet(field.key, value);
          present[field.key] = value.length > 0;
          input.value = "";
          input.placeholder = value ? "••••••••  (stored)" : field.placeholder;
          dotEl.style.background = value ? "#22c55e" : "#f4505e";
        } catch {
          dotEl.style.background = "#f5a524";
        }
      });
      rows.append(
        h("div", { class: "row" },
          h("label", { style: "min-width:104px", text: field.label }),
          input, saveBtn, dotEl,
        ),
      );
    }

    list.append(
      h("div", { style: "display:flex;gap:12px;align-items:flex-start" },
        h("div", { style: "display:flex;align-items:center;gap:8px;min-width:132px;padding-top:4px" },
          sw,
          h("i", { class: "dot", style: `background:${def.color}` }),
          h("span", { style: "font-size:12.5px", text: def.name }),
        ),
        rows,
      ),
    );
  }

  updateNote();
  return h("section", {}, h("h2", {}, h("span", { text: "Integrations" })), note, list);
}

// ── General section ───────────────────────────────────────────────────────────

function generalSection(): HTMLElement {
  const volume = h("input", {
    type: "range", min: "0", max: "0.2", step: "0.005",
    value: String(settings.soundVolume),
  }) as HTMLInputElement;
  volume.addEventListener("input", () => {
    settings.soundVolume = Number(volume.value);
    void save();
  });

  const autoClose = h("input", {
    type: "number", min: "5", max: "120", step: "1",
    value: String(Math.round(settings.autoCloseInterval)),
    style: "width:72px",
  }) as HTMLInputElement;
  autoClose.addEventListener("change", () => {
    settings.autoCloseInterval = Math.max(5, Math.min(120, Number(autoClose.value) || 15));
    autoClose.value = String(settings.autoCloseInterval);
    void save();
  });

  const screen = h("select", {}) as HTMLSelectElement;
  screen.append(
    h("option", { value: "primary", text: "Main display" }),
    h("option", { value: "cursor", text: "Display under the cursor" }),
  );
  screen.value = settings.screen;
  screen.addEventListener("change", () => {
    settings.screen = screen.value as Settings["screen"];
    // Choosing here overrides a screen it was dragged to.
    settings.dockScreen = "";
    void save();
  });

  const startOn = h("select", {}) as HTMLSelectElement;
  startOn.append(
    h("option", { value: "main", text: "On the main screen" }),
    h("option", { value: "last", text: "Where I left it" }),
  );
  startOn.value = settings.startOnMainScreen ? "main" : "last";
  startOn.addEventListener("change", () => {
    settings.startOnMainScreen = startOn.value === "main";
    void save();
  });
  syncPlace = () => {
    startOn.value = settings.startOnMainScreen ? "main" : "last";
  };

  return h(
    "section",
    {},
    h("h2", {}, h("span", { text: "General" })),
    h("div", { class: "row" },
      h("label", { text: "Sound" }),
      toggle(settings.soundEnabled, (v) => { settings.soundEnabled = v; void save(); }),
      volume,
    ),
    h("div", { class: "row" },
      h("label", { text: "Auto-close" }),
      autoClose,
      h("span", { class: "hint", text: "seconds after you leave the island" }),
    ),
    h("div", { class: "row" },
      h("label", { text: "Island lives on" }),
      screen,
    ),
    h("div", { class: "hint", text: "Drag the island to move it to another screen." }),
    h("div", { class: "row" },
      h("label", { text: "When it starts" }),
      startOn,
    ),
    h("div", { class: "row" },
      h("label", { text: "Launch at startup" }),
      toggle(settings.autostart, (v) => { settings.autostart = v; void save(); }),
    ),
  );
}

// ── Boot ──────────────────────────────────────────────────────────────────────

async function main() {
  const boot = await Bridge.boot();
  if (boot) {
    settings = { ...settings, ...boot.settings };
    version = boot.version;
    if (boot.platform === "linux") keyStore = "your keyring (KWallet / GNOME Keyring)";
  }
  const status = (await Bridge.hooksStatus()) ?? {
    installed: false, settingsPath: "", hookPath: "", hookReady: false,
  };

  const keys = [
    "stripe-api-key", "github-token", "vercel-token",
    "n8n-url", "n8n-api-key", "resend-api-key", "notion-api-key", "calcom-api-key",
  ];
  const present: Record<string, boolean> = {};
  for (const k of keys) present[k] = (await Bridge.secretPresent(k)) ?? false;

  clear(root);
  root.append(
    h("h1", {}, h("span", { text: "Coucou" }), h("span", { class: "version", text: version })),
    claudeSection(status),
    aiSection(),
    integrationsSection(present),
    generalSection(),
    h("div", {
      class: "hint",
      text: "No telemetry. Network requests only go to the services you configure yourself.",
    }),
  );

  void onEvent<Settings>("settings-changed", (s) => {
    settings = { ...settings, ...s };
    syncMode();
    syncPlace();
  });
}

void main();
