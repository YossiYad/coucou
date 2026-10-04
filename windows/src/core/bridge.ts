// Thin wrapper over the Tauri commands/events. Every call is a no-op when the
// page is opened in a plain browser, so the island can be iterated on with
// `npm run dev` alone.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { Preview, Settings } from "./state";

export const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!IS_TAURI) return null;
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    console.error(`[coucou] ${cmd} failed`, err);
    return null;
  }
}

export interface BootInfo {
  settings: Settings;
  /** Logical screen rect of the monitor the island lives on. */
  screen: { x: number; y: number; width: number; height: number; scale: number };
  version: string;
  hookPath: string;
  /** "windows" or "linux". */
  platform: string;
}

export const Bridge = {
  boot: () => call<BootInfo>("boot"),

  saveSettings: (settings: Settings) => call<void>("save_settings", { settings }),

  /** Shrink the window down to the invisible wake strip (hidden) or back to full. */
  setCollapsed: (collapsed: boolean) => call<void>("set_collapsed", { collapsed }),

  /**
   * Pushes the island shape in window coordinates. Rust flips click-through from
   * its own cursor poll, so the flag is never a frame behind a click.
   */
  setIslandRect: (x: number, y: number, width: number, height: number) =>
    call<void>("set_island_rect", { x, y, width, height }),

  /** Give the window keyboard focus (chat field) and take it away again. */
  focusWindow: (focused: boolean) => call<void>("focus_window", { focused }),

  reposition: () => call<void>("reposition"),

  openUrl: (url: string) => call<void>("open_url", { url }),

  /** "Open terminal" → opens the folder in VS Code when `code` is on PATH. */
  openInVSCode: (path: string | null) => call<boolean>("open_in_vscode", { path }),

  quit: () => call<void>("quit_app"),

  openSettingsWindow: () => call<void>("open_settings_window"),

  /** Writes to coucou.log (%LOCALAPPDATA%\Coucou or ~/.local/share/coucou), next to the Rust lines. */
  log: (message: string) => call<void>("log_line", { message }),

  // ── Claude Code hooks ─────────────────────────────────────────────────────
  hooksStatus: () => call<HookStatus>("hooks_status"),
  /** Diff to show before anything is written. `install: false` previews removal. */
  hooksPreview: (install: boolean) => callOrThrow<HookPreview>("hooks_preview", { install }),
  /**
   * Writes ~/.claude/settings.json — only ever after an explicit click, and only
   * when the file still matches the preview the user looked at.
   */
  hooksApply: (install: boolean, fingerprint: string) =>
    callOrThrow<string>("hooks_apply", { install, fingerprint }),

  approvalDecision: (requestId: string, decision: "allow" | "deny") =>
    call<void>("approval_decision", { requestId, decision }),
  /** "The card is up" — until this lands the relay only waits a moment. */
  approvalAck: (requestId: string) => call<void>("approval_ack", { requestId }),
  /** "Nobody can act on this" — Claude Code asks in the terminal right away. */
  approvalDecline: (requestId: string) => call<void>("approval_decline", { requestId }),

  // ── Chat, files, secrets ──────────────────────────────────────────────────
  /** One chat turn. The API key and any file bytes never leave Rust. */
  chatSend: (query: string, context: ChatContext | null) =>
    callOrThrow<{ text: string; note: string | null }>("chat_send", { query, context }),
  chatReset: () => call<void>("chat_reset"),
  /** The models a provider offers, asked from the provider itself. */
  aiModels: (provider: string, start = false) => callOrThrow<ModelInfo[]>("ai_models", { provider, start }),
  /** The microphone: listen, then stop and get the words as text. */
  voiceStart: () => callOrThrow<void>("voice_start"),
  voiceStop: () => callOrThrow<string>("voice_stop"),
  voiceCancel: () => call<void>("voice_cancel"),
  listMicrophones: () => call<{ id: string; label: string }[]>("list_microphones"),
  /** Reads text out loud, and stops reading. */
  speak: (text: string) => call<void>("speak", { text }),
  stopSpeaking: () => call<void>("stop_speaking"),
  /** Hands the island to the window manager to drag; it snaps to an edge after. */
  startDrag: () => call<void>("start_drag"),
  /** Ends a command the chat model is running. */
  stopCommand: (step: number) => call<void>("stop_command", { step }),
  /** What a Claude Code edit will do to its file, as a diff. */
  changePreview: (tool: string, input: Record<string, unknown>) =>
    call<Preview | null>("change_preview", { tool, input }),
  /** Allow / Deny a file change the chat model asked for. */
  toolDecision: (id: number, allow: boolean) => call<void>("tool_decision", { id, allow }),
  /** Saves an image pasted into the chat in the inbox; its bytes go as they are. */
  ingestPasted: async (bytes: Uint8Array, type: string): Promise<DroppedFile> => {
    if (!IS_TAURI) throw new Error("Pasting needs the app");
    return invoke<DroppedFile>("ingest_pasted", bytes, { headers: { "x-type": type } });
  },
  /** The clipboard's image, read by the app when the web view does not pass it on. */
  pasteClipboardImage: () => callOrThrow<DroppedFile>("paste_clipboard_image"),
  /** A screenshot of the screen (the mouse's monitor, or all), saved in the inbox. */
  captureScreen: (all: boolean) => callOrThrow<DroppedFile>("capture_screen", { all }),
  /** Opens a received file (a pasted image) in its usual app. */
  openInboxFile: (path: string) => callOrThrow<void>("open_inbox_file", { path }),
  /** Copies a dropped file into the inbox. */
  ingestFile: (path: string) => callOrThrow<DroppedFile>("ingest_file", { path }),
  /** Only ever tells you whether a key exists — never its value. */
  secretPresent: (key: string) => call<boolean>("secret_present", { key }),
  secretSet: (key: string, value: string) => callOrThrow<void>("secret_set", { key, value }),
  secretClear: (key: string) => callOrThrow<void>("secret_clear", { key }),

  // ── Integrations ──────────────────────────────────────────────────────────
  refreshIntegration: (id: string) => call<void>("refresh_integration", { id }),
  /** Opens the configured n8n instance in the browser. */
  openN8n: () => call<void>("open_n8n"),

  /** Tray → Pause. Stops the integration pollers, not just the island. */
  setPaused: (paused: boolean) => call<void>("set_paused", { paused }),
};

export interface IntegrationUpdate {
  id: string;
  data: Record<string, unknown>;
  error: string | null;
  event: { success: boolean; label: string; detail: string | null } | null;
}

export type ChatContext =
  | { kind: "file"; name: string; path: string; original?: string }
  | { kind: "window"; appName: string; title: string; url?: string };

export interface ModelInfo {
  id: string;
  label: string;
}

export interface DroppedFile {
  /** The image as a data URL, when it is one, for the chat's thumbnail. */
  preview?: string;
  name: string;
  path: string;
  size: number;
}

export interface HookStatus {
  installed: boolean;
  settingsPath: string;
  hookPath: string;
  hookReady: boolean;
}

export interface HookPreview {
  diff: string;
  backup: string;
  settingsPath: string;
  /** Hand back to hooksApply so only the reviewed diff is ever written. */
  fingerprint: string;
}

/** Same as `call`, but surfaces the error so the UI can show what went wrong. */
async function callOrThrow<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!IS_TAURI) throw new Error("not running inside Coucou");
  return invoke<T>(cmd, args);
}

export type BridgeEvent =
  | { name: "cursor"; payload: { x: number; y: number } }
  | { name: "tray"; payload: string }
  | { name: "hook"; payload: Record<string, unknown> }
  | { name: "screen-changed"; payload: null }
  | { name: "pointer-left"; payload: null }
  | { name: "pointer-entered"; payload: null };

export interface DragDropPayload {
  type: "enter" | "over" | "drop" | "leave";
  paths?: string[];
  /** Physical pixels, relative to the window. Only meaningful on Linux. */
  position?: { x: number; y: number };
}

/** Files dragged onto the island. Only reaches us when the window takes the mouse. */
export async function onDragDrop(handler: (e: DragDropPayload) => void) {
  if (!IS_TAURI) return () => {};
  return getCurrentWebview().onDragDropEvent((event) => {
    handler(event.payload as DragDropPayload);
  });
}

export async function onEvent<T>(name: string, handler: (payload: T) => void) {
  if (!IS_TAURI) return () => {};
  return listen<T>(name, (e) => handler(e.payload));
}
