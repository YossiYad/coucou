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

/** Shows an image large over the chat; Esc, a click outside or × closes it. */
function buildLightbox() {
  const img = h("img", { class: "lightbox-img", alt: "" }) as HTMLImageElement;
  const full = h("button", { class: "lightbox-btn", text: "Open full size" });
  const close = h("button", { class: "lightbox-x", title: "Close" }, svg(ICONS.xmark, 10));
  const el = h("div", { class: "lightbox" }, img, h("div", { class: "lightbox-bar" }, full, close));
  let path: string | undefined;
  /** Set by the chat view: the island resizes when the picture opens or closes. */
  let resized = () => {};
  const hide = () => {
    el.classList.remove("open");
    State.imageOpen = false;
    resized();
  };
  el.addEventListener("click", (e) => {
    if (e.target === el) hide();
  });
  close.addEventListener("click", hide);
  // Esc closes the picture first, before it could close the whole island.
  document.addEventListener(
    "keydown",
    (e) => {
      if (e.key !== "Escape" || !el.classList.contains("open")) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      hide();
    },
    true,
  );
  full.addEventListener("click", () => {
    if (path) void Bridge.openInboxFile(path).catch(() => {});
  });
  return {
    el,
    show(src: string, file?: string) {
      img.src = src;
      path = file;
      full.style.display = file ? "" : "none";
      el.classList.add("open");
      State.imageOpen = true;
      resized();
    },
    onResize(fn: () => void) {
      resized = fn;
    },
  };
}

const lightbox = buildLightbox();

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    return h(
      "div",
      { class: "chat-row user" },
      h(
        "div",
        { class: "user-turn" },
        message.image
          ? h("img", {
              class: "bubble-img",
              src: message.image,
              alt: "",
              title: "Click to enlarge",
              onclick: () => lightbox.show(message.image!, message.imagePath),
            })
          : null,
        h("div", { class: "bubble", dir: "auto", text: message.content }),
      ),
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
function contextChip(
  label: string,
  preview: string | undefined,
  path: string,
  onRemove: (() => void) | null,
): HTMLElement {
  const chip = h(
    "div",
    { class: "chip" },
    preview
      ? h("img", {
          class: "chip-thumb",
          src: preview,
          alt: "",
          title: "Click to enlarge",
          onclick: () => lightbox.show(preview, path),
        })
      : h("i", { class: "chip-dot" }),
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

/** A microphone, for talking to it. */
const MIC_ICON = "M12 3.5a2.6 2.6 0 0 0-2.6 2.6v5.4a2.6 2.6 0 0 0 5.2 0V6.1A2.6 2.6 0 0 0 12 3.5z M6.5 11a5.5 5.5 0 0 0 11 0 M12 16.5v4";

/** A monitor outline, for the screen sharing button. */
const SCREEN_ICON = "M3 4.5h18v11.5H3z M9 20.5h6 M12 16v4.5";

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
  lightbox.onResize(onHeightChange);
  const stepsBtn = h("button", { class: "steps-btn", title: "What it did", onclick: () => actions.setView("work") }, svg(ICONS.stack, 12));
  // Screen sharing: while on, every question takes a screenshot along.
  const screenBtn = h("button", { class: "screen-btn", title: "Share your screen with each question" }, svg(SCREEN_ICON, 13, { stroke: 1.8 }));
  screenBtn.addEventListener("mousedown", (e) => e.preventDefault());
  screenBtn.addEventListener("click", () => {
    State.settings.screenSharing = !State.settings.screenSharing;
    void Bridge.saveSettings(State.settings);
    Sound.play("blip");
    State.notify();
  });
  // Talking: click to listen, click again to send what was said.
  const micBtn = h("button", { class: "mic-btn", title: "Talk to it" }, svg(MIC_ICON, 13, { stroke: 1.8 }));
  let listening = false;
  let spokenTurn = false;
  const stopListening = async (send: boolean) => {
    listening = false;
    updateEngaged();
    micBtn.classList.remove("on", "busy");
    input.placeholder = State.chatHistory.length === 0 ? "Ask me anything…" : "Continue…";
    if (!send) {
      void Bridge.voiceCancel();
      updateEngaged();
      return;
    }
    micBtn.classList.add("busy");
    try {
      const said = await Bridge.voiceStop();
      input.value = said;
      spokenTurn = true;
      void submit();
    } catch (err) {
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      actions.setView("note");
    } finally {
      micBtn.classList.remove("busy");
    }
  };
  micBtn.addEventListener("mousedown", (e) => e.preventDefault());
  micBtn.addEventListener("click", async () => {
    if (listening) return void stopListening(true);
    void Bridge.stopSpeaking();
    try {
      await Bridge.voiceStart();
      listening = true;
      updateEngaged();
      micBtn.classList.add("on");
      input.placeholder = "Listening… click the microphone when you're done";
      Sound.play("blip");
    } catch (err) {
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      actions.setView("note");
    }
  });
  const bar = h("div", { class: "chat-bar" }, picker.button, input, micBtn, stepsBtn, send);
  // Screen sharing sits outside the box, on its left: a switch, not a tool.
  const barRow = h("div", { class: "chat-bar-row" }, screenBtn, bar);
  /** Live guidance: "I did that", with a fresh look at the screen. */
  const nextStep = h("button", { class: "go-ahead next-step" }, h("span", { text: "Done, next step" }));
  nextStep.addEventListener("click", () => {
    input.value = "I did that. What's the next step?";
    void submit();
  });
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
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body" }, chipRow, log, barRow), picker.menu, lightbox.el),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");

  let sending = false;
  let renderedKey = "";

  /** Typing, a half-written question, or an answer on its way: keep the island open. */
  /** What really holds the island open: a question being written, waited for
   *  or spoken. Focus in an empty box only buys the usual close delay, or one
   *  answer left the island open for good (the box takes focus after it). */
  function updateEngaged() {
    State.chatEngaged = sending || listening || input.value.trim() !== "";
  }
  const touched = () => {
    updateEngaged();
    const grace = performance.now() + State.settings.autoCloseInterval * 1000;
    State.chatReadUntil = Math.max(State.chatReadUntil, grace);
  };
  input.addEventListener("input", touched);
  input.addEventListener("focus", touched);
  input.addEventListener("blur", updateEngaged);

  async function submit() {
    const query = input.value.trim();
    if (!query || sending) return;
    input.value = "";
    sending = true;
    updateEngaged();
    Sound.play("send");

    // Sharing the screen: the question goes with what is on it now, unless
    // the user attached something of their own.
    if (State.settings.screenSharing && !(State.droppedFile && !State.droppedFile.sent)) {
      try {
        const shot = await Bridge.captureScreen(State.settings.screenScope === "all");
        State.droppedFile = { name: shot.name, path: shot.path, original: shot.path, sent: false, preview: shot.preview };
      } catch (err) {
        State.noteMessage = String(err).replace(/^Error:\s*/, "");
      }
    }

    const unsent = State.droppedFile && !State.droppedFile.sent ? State.droppedFile : null;
    State.chatHistory.push({
      id: nextId++,
      role: "user",
      content: query,
      image: unsent?.preview,
      imagePath: unsent?.preview ? unsent.path : undefined,
    });
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
        // A spoken question gets a spoken answer.
        if (spokenTurn && State.settings.speakAnswers) void Bridge.speak(reply.text);
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
      spokenTurn = false;
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
  const attach = (file: { name: string; path: string; preview?: string }, local?: string) => {
    // A sent image stays on screen in its message; only an unsent one is let go.
    const old = State.droppedFile;
    if (old && !old.sent && old.preview?.startsWith("blob:")) URL.revokeObjectURL(old.preview);
    const preview = file.preview ?? local;
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
    if (key.key === "Escape" && listening) {
      // Escape while listening throws the recording away.
      e.preventDefault();
      void stopListening(false);
      return;
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
                if (file.preview?.startsWith("blob:")) URL.revokeObjectURL(file.preview);
                State.droppedFile = null;
                State.notify();
              };
          chipRow.append(contextChip(file.name, file.preview, file.path, remove));
        }
      }

      const thinking = State.stateOverride === "thinking";
      const offerPlan = !thinking && State.answeredInPlan && State.chatHistory.at(-1)?.role === "assistant";
      const offerNext =
        !thinking && !offerPlan && State.settings.screenSharing && State.chatHistory.at(-1)?.role === "assistant";
      const key = `${State.chatHistory.length}|${thinking}|${thinking ? State.toolActivity ?? "" : ""}|${offerPlan}|${offerNext}`;
      if (key !== renderedKey) {
        renderedKey = key;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (thinking) log.append(typingDots(State.toolActivity));
        if (offerPlan) log.append(h("div", { class: "chat-row" }, goAhead));
        if (offerNext) log.append(h("div", { class: "chat-row" }, nextStep));
        log.scrollTop = log.scrollHeight;
      }
      picker.sync();
      screenBtn.classList.toggle("on", State.settings.screenSharing);
      screenBtn.title = State.settings.screenSharing
        ? "Screen sharing is on: each question takes a screenshot. Click to stop."
        : "Share your screen with each question";
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
