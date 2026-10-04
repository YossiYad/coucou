// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView from
// IslandViewContent.swift.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge, type ChatContext } from "../core/bridge";
import { Sound } from "../core/sound";
import { PERMISSION_MODES, State, type ChatMessage, type PermissionMode } from "../core/state";
import { chatFinished } from "../island/work";
import type { ViewActions, ViewHost } from "./views";

let nextId = 1;

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    return h(
      "div",
      { class: "chat-row user" },
      h("div", { class: "bubble", dir: "auto", text: message.content }),
    );
  }
  // dir="auto": Hebrew or Arabic reads right to left, punctuation included.
  const reply = h("div", { class: "reply", dir: "auto", text: message.content });
  if (!message.note) return h("div", { class: "chat-row" }, reply);
  return h("div", { class: "chat-row" }, h("div", {}, reply, h("div", { class: "reply-note", text: message.note })));
}

function typingDots(activity: string | null): HTMLElement {
  return h(
    "div",
    { class: "chat-row" },
    h("div", { class: "typing" }, h("i"), h("i"), h("i")),
    // What the model is doing with its tools: "Reading report.pdf…".
    activity
      ? h("div", { class: "reply", dir: "auto", style: "opacity:.6;font-size:12px;margin-inline-start:8px", text: activity })
      : null,
  );
}

/** The coloured chip showing what the question is about (a dropped or
 *  pasted file), with the image itself when it is one, and an × until sent. */
function contextChip(label: string, preview: string | undefined, onRemove: (() => void) | null): HTMLElement {
  const chip = h(
    "div",
    { class: "chip" },
    preview ? h("img", { class: "chip-thumb", src: preview, alt: "" }) : h("i", { class: "chip-dot" }),
    h("span", { text: label }),
    onRemove ? h("button", { class: "chip-x", title: "Remove", onclick: onRemove }, svg(ICONS.xmark, 8)) : null,
  );
  requestAnimationFrame(() => chip.classList.add("settled"));
  return chip;
}

/** A file:// link from a file manager's copy, as a path. */
function pastedPath(data: DataTransfer): string | null {
  const text = (data.getData("text/uri-list") || data.getData("text/plain") || "").trim();
  const first = text.split(/\r?\n/).find((l) => l && !l.startsWith("#")) ?? "";
  if (!first.startsWith("file://")) return null;
  try {
    return decodeURIComponent(new URL(first).pathname);
  } catch {
    return null;
  }
}

/** Sets the permission mode and keeps it, like Claude Code's Shift+Tab. */
function setMode(mode: PermissionMode) {
  const current = State.settings.permissionMode;
  if (current !== "plan") lastActingMode = current;
  State.settings.permissionMode = mode;
  void Bridge.saveSettings(State.settings);
  State.notify();
}

/** Where "Go ahead" returns to after a plan. */
let lastActingMode: PermissionMode = "manual";

/** The mode button and its menu, listing the modes like Claude Code does. */
function buildModePicker(): { button: HTMLElement; menu: HTMLElement; sync(): void } {
  const label = h("span");
  const button = h("button", { class: "mode-btn", title: "Who approves changes (Shift+Tab)" }, label);
  const menu = h("div", { class: "mode-menu" });
  const items = PERMISSION_MODES.map((m, i) => {
    const item = h(
      "button",
      { class: "mode-item" },
      h("span", { class: "mode-text" }, h("b", { text: m.label }), h("span", { text: m.hint })),
      h("span", { class: "mode-check" }, svg(ICONS.check, 11)),
      h("span", { class: "mode-key", text: String(i + 1) }),
    );
    item.addEventListener("mousedown", (e) => e.preventDefault()); // keep the input focused
    item.addEventListener("click", () => {
      setMode(m.id);
      menu.classList.remove("open");
    });
    menu.append(item);
    return item;
  });
  button.addEventListener("mousedown", (e) => e.preventDefault());
  button.addEventListener("click", () => menu.classList.toggle("open"));
  return {
    button,
    menu,
    sync() {
      const mode = State.settings.permissionMode;
      label.textContent = PERMISSION_MODES.find((m) => m.id === mode)?.label ?? "Manual";
      button.dataset.mode = mode;
      button.style.display = State.settings.toolsEnabled ? "" : "none";
      items.forEach((item, i) => item.classList.toggle("on", PERMISSION_MODES[i].id === mode));
    },
  };
}

export function buildPrompt(onHeightChange: () => void, actions: ViewActions): ViewHost {
  const chipRow = h("div", { class: "chip-row" });
  const log = h("div", { class: "chat-log" });
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Ask me anything…",
    spellcheck: "false",
    dir: "auto",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Send" }, svg(ICONS.arrowUp, 11));
  const picker = buildModePicker();
  const stepsBtn = h("button", { class: "steps-btn", title: "What it did", onclick: () => actions.setView("work") }, svg(ICONS.stack, 12));
  const bar = h("div", { class: "chat-bar" }, picker.button, input, stepsBtn, send);
  /** After a plan: run it in the mode used before Plan. */
  const goAhead = h("button", { class: "go-ahead" }, h("span", { text: "Go ahead" }));
  goAhead.addEventListener("click", () => {
    setMode(lastActingMode);
    input.value = "Go ahead with the plan.";
    void submit();
  });

  const el = h(
    "div",
    { class: "view" },
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body" }, chipRow, log, bar), picker.menu),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");

  let sending = false;
  let renderedKey = "";

  /** Typing, a half-written question, or an answer on its way: keep the island open. */
  function updateEngaged() {
    State.chatEngaged = sending || input.value.trim() !== "" || document.activeElement === input;
  }
  for (const event of ["input", "focus", "blur"]) input.addEventListener(event, updateEngaged);

  async function submit() {
    const query = input.value.trim();
    if (!query || sending) return;
    input.value = "";
    sending = true;
    updateEngaged();
    Sound.play("send");

    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    // A file goes with the first question after it was dropped or pasted.
    const file = State.droppedFile;
    const context: ChatContext | null =
      file && !file.sent ? { kind: "file", name: file.name, path: file.path, original: file.original } : null;
    if (file) file.sent = true;

    // The conversation may be cleared while the answer is on its way (the
    // island hid): it then belongs to nothing on screen.
    const conversation = State.chatHistory;
    State.toolActivity = null;
    const planning = State.settings.toolsEnabled && State.settings.permissionMode === "plan";
    State.answeredInPlan = false;
    try {
      const reply = await Bridge.chatSend(query, context);
      State.stateOverride = null;
      chatFinished();
      if (State.view === "work") actions.setView("prompt");
      if (State.chatHistory === conversation) {
        State.answeredInPlan = planning;
        // However long the wait, the answer gets a full close delay to be read.
        State.chatReadUntil = performance.now() + State.settings.autoCloseInterval * 1000;
        State.chatHistory.push({ id: nextId++, role: "assistant", content: reply.text, note: reply.note ?? undefined });
        Sound.play("finish");
      }
    } catch (err) {
      State.stateOverride = null;
      chatFinished();
      if (State.chatHistory === conversation) {
        State.noteMessage = String(err).replace(/^Error:\s*/, "");
        State.view = "note";
        Sound.play("error");
      }
    } finally {
      State.toolActivity = null;
      sending = false;
      updateEngaged();
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  send.addEventListener("click", () => void submit());
  // Ctrl+V of a screenshot, a copied image or a file copied in the file
  // manager attaches it to the next question; plain text pastes as text.
  const attach = (file: { name: string; path: string }, preview?: string) => {
    const old = State.droppedFile?.preview;
    if (old && old !== preview) URL.revokeObjectURL(old);
    State.droppedFile = { name: file.name, path: file.path, original: file.path, sent: false, preview };
    Sound.play("blip");
    State.notify();
    onHeightChange();
  };
  const pasteFailed = (err: unknown) => {
    State.noteMessage = String(err).replace(/^Error:\s*/, "");
    actions.setView("note");
  };
  input.addEventListener("paste", (e) => {
    const data = (e as ClipboardEvent).clipboardData;
    if (!data) return;
    const image = Array.from(data.items).find((i) => i.kind === "file" && i.type.startsWith("image/"));
    const blob = image?.getAsFile();
    if (blob) {
      e.preventDefault();
      void blob
        .arrayBuffer()
        .then((buf) => Bridge.ingestPasted(new Uint8Array(buf), blob.type))
        .then((file) => attach(file, URL.createObjectURL(blob)))
        .catch(pasteFailed);
      return;
    }
    const path = pastedPath(data);
    if (path) {
      e.preventDefault();
      void Bridge.ingestFile(path).then((file) => attach(file)).catch(pasteFailed);
      return;
    }
    if (!data.getData("text/plain")) {
      // Nothing the web view understood, but the clipboard may still hold an
      // image (WebKitGTK does not pass every one on): ask the app for it.
      e.preventDefault();
      void Bridge.pasteClipboardImage().then((file) => attach(file)).catch(pasteFailed);
    }
  });

  input.addEventListener("keydown", (e) => {
    const key = e as KeyboardEvent;
    if (key.key === "Enter") {
      e.preventDefault();
      void submit();
    }
    if (key.key === "Tab" && key.shiftKey && State.settings.toolsEnabled) {
      // Shift+Tab cycles the modes, as in Claude Code.
      e.preventDefault();
      const i = PERMISSION_MODES.findIndex((m) => m.id === State.settings.permissionMode);
      setMode(PERMISSION_MODES[(i + 1) % PERMISSION_MODES.length].id);
    }
    if (picker.menu.classList.contains("open") && /^[1-4]$/.test(key.key)) {
      // The numbers the menu shows pick a mode, as in Claude Code.
      e.preventDefault();
      setMode(PERMISSION_MODES[Number(key.key) - 1].id);
      picker.menu.classList.remove("open");
    }
    if (key.key === "Escape" && picker.menu.classList.contains("open")) {
      e.preventDefault();
      picker.menu.classList.remove("open");
    }
    e.stopPropagation(); // Escape closes the island, not the chat
  });

  return {
    el,
    sync() {
      const file = State.droppedFile;
      const wantChip = file ? `${file.name}|${file.sent ? 1 : 0}` : "";
      if (chipRow.dataset.label !== wantChip) {
        chipRow.dataset.label = wantChip;
        clear(chipRow);
        if (file) {
          const remove = file.sent
            ? null
            : () => {
                if (file.preview) URL.revokeObjectURL(file.preview);
                State.droppedFile = null;
                State.notify();
              };
          chipRow.append(contextChip(file.name, file.preview, remove));
        }
      }

      const thinking = State.stateOverride === "thinking";
      const offerPlan = !thinking && State.answeredInPlan && State.chatHistory.at(-1)?.role === "assistant";
      const key = `${State.chatHistory.length}|${thinking}|${thinking ? State.toolActivity ?? "" : ""}|${offerPlan}`;
      if (key !== renderedKey) {
        renderedKey = key;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (thinking) log.append(typingDots(State.toolActivity));
        if (offerPlan) log.append(h("div", { class: "chat-row" }, goAhead));
        log.scrollTop = log.scrollHeight;
      }
      picker.sync();
      const work = State.work;
      stepsBtn.style.display = work?.owner === "chat" && work.steps.length ? "" : "none";

      input.placeholder = State.chatHistory.length === 0 ? "Ask me anything…" : "Continue…";
      input.disabled = sending;
    },
    focus() {
      input.focus();
      input.select();
    },
  };
}
