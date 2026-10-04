// Feeds the work view: the chat model's steps and changes from Rust, and
// Claude Code's from its hooks (see hooks.ts). One session at a time, the
// latest owner's.

import { onEvent } from "../core/bridge";
import { Sound } from "../core/sound";
import {
  PERMISSION_MODES,
  State,
  providerName,
  type DiffLine,
  type Preview,
  type StepState,
  type WorkPanel,
  type WorkSession,
} from "../core/state";
import type { Island } from "./island";

/** A session that has been quiet this long is shown as done. */
const QUIET_MS = 8000;

interface StepEvent {
  id: number;
  state: StepState;
  tool?: string;
  target?: string;
  path?: string;
  lines?: DiffLine[];
}

interface ChangeEvent {
  id: number;
  step: number;
  title: string;
  file: string;
  path: string;
  preview: Preview;
  waiting: boolean;
}

/** The session for `owner`, started afresh when another one held the view. */
export function session(owner: string, who: string, sub: string): WorkSession {
  let work = State.work;
  if (!work || work.owner !== owner || !work.active) {
    work = { owner, who, sub, steps: [], panel: null, active: true, at: performance.now() };
    State.work = work;
  }
  work.who = who;
  work.sub = sub;
  work.active = true;
  work.at = performance.now();
  return work;
}

export function addStep(work: WorkSession, id: number, tool: string, target: string) {
  work.steps.push({ id, tool, target, state: "running" });
  if (work.steps.length > 30) work.steps.shift();
}

export function setStep(work: WorkSession, id: number, state: StepState) {
  const step = work.steps.find((s) => s.id === id);
  if (step) step.state = state;
}

export function setPanel(work: WorkSession, panel: WorkPanel) {
  // A change waiting for a click is never pushed aside by a read.
  if (work.panel && (work.panel.approvalId != null || work.panel.hookRequestId)) return;
  work.panel = panel;
}

/** Shows the work view when the island is open on something it replaces. */
export function showWork(island: Island) {
  if (State.mode !== "expanded") return;
  if (State.view === "prompt" || State.view === "overview" || State.view === "empty") island.setView("work");
}

/** Lines as a plain text preview. */
export function plain(lines: string[]): Preview {
  return { kind: "text", lines: lines.map((text, i) => ({ old: i + 1, new: i + 1, mark: "same", text })) };
}

function chatSession(): WorkSession {
  const mode = PERMISSION_MODES.find((m) => m.id === State.settings.permissionMode)?.label ?? "Manual";
  return session("chat", providerName(State.settings), mode);
}

export async function registerWorkHandlers(island: Island) {
  await onEvent<StepEvent>("agent-step", (e) => {
    if (e.state === "running") {
      const work = chatSession();
      addStep(work, e.id, e.tool ?? "", e.target ?? "");
      showWork(island);
    } else {
      const work = State.work;
      if (!work || work.owner !== "chat") return;
      setStep(work, e.id, e.state);
      work.at = performance.now();
      const step = work.steps.find((s) => s.id === e.id);
      if (e.lines?.length && step) {
        setPanel(work, { file: step.target, path: e.path ?? step.target, preview: { kind: "text", lines: e.lines } });
      }
    }
    State.notify();
  });

  await onEvent<ChangeEvent>("agent-change", (e) => {
    const work = chatSession();
    if (work.panel) work.panel.approvalId = undefined;
    work.panel = {
      file: e.file,
      path: e.path,
      preview: e.preview,
      approvalId: e.waiting ? e.id : undefined,
      note: e.title,
      changeId: e.id,
      stepId: e.step,
      commandStep: e.file === "Terminal" ? e.step : undefined,
    };
    if (e.waiting) {
      setStep(work, e.step, "waiting");
      // Waits for a decision, like a Claude Code permission: never closes on its own.
      State.isPinned = true;
      Sound.play("approval");
      island.alert("work");
    } else {
      showWork(island);
    }
    State.notify();
  });

  // The chosen model could not answer; another one carries on.
  await onEvent<{ name: string }>("ai-fallback", ({ name }) => {
    const work = State.work;
    if (work?.owner === "chat") work.who = name;
    State.toolActivity = `Asking ${name}…`;
    State.notify();
  });

  // A command's output, as it arrives.
  await onEvent<{ step: number; lines: DiffLine[] }>("agent-output", ({ step, lines }) => {
    const work = State.work;
    if (!work || work.owner !== "chat" || work.panel?.commandStep !== step) return;
    work.panel.preview = { kind: "text", lines };
    work.at = performance.now();
    State.notify();
  });

  await onEvent<{ id: number; allowed: boolean }>("agent-change-done", ({ id, allowed }) => {
    const panel = State.work?.panel;
    if (panel && panel.changeId === id) {
      panel.approvalId = undefined;
      panel.outcome = allowed ? "applied" : "declined";
      // Approved: the step carries on (a command now runs) until Rust says it is done.
      const step = State.work?.steps.find((s) => s.id === panel.stepId);
      if (allowed && step?.state === "waiting") step.state = "running";
    }
    // Nothing left waiting for a click: the island may close on its own again,
    // whichever change this was (a pin left behind kept it open for good).
    const stillWaiting = State.work?.panel?.approvalId != null || !!State.work?.panel?.hookRequestId || !!State.pendingApproval;
    if (!stillWaiting) {
      State.isPinned = false;
      island.dropPin();
    }
    State.notify();
  });

  // A session nobody has heard from in a while is over (Claude Code may stop
  // without a Stop hook, the chat may have failed).
  window.setInterval(() => {
    const work = State.work;
    if (!work?.active || performance.now() - work.at < QUIET_MS) return;
    if (work.panel?.approvalId != null || work.panel?.hookRequestId) return;
    if (work.owner === "chat" && State.stateOverride === "thinking") return;
    work.active = false;
    State.notify();
  }, 1000);
}

/** The chat turn is over: its session is done. */
export function chatFinished() {
  const work = State.work;
  if (!work || work.owner !== "chat") return;
  work.active = false;
  if (work.panel) work.panel.approvalId = undefined;
}
