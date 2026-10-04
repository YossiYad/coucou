// Work view: an agent at work, step by step, with the change it is making
// drawn as it will land - a diff for text and code, the rows and columns for a
// spreadsheet, the paragraphs for a Word document. The same view serves the
// chat model (any provider) and Claude Code through its hooks.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge } from "../core/bridge";
import { State, type DiffLine, type Preview, type WorkPanel, type WorkStep } from "../core/state";
import type { ViewActions, ViewHost } from "./views";

/** Steps listed at most; older ones scroll away. */
const MAX_STEPS = 5;

const VERBS: Record<string, string> = {
  search_files: "Search",
  list_folder: "List",
  read_file: "Read",
  create_file: "Create",
  edit_spreadsheet: "Edit",
  edit_document: "Edit",
  edit_file: "Edit",
  read_web_page: "Fetch",
  search_web: "Search web",
  open: "Open",
  run_command: "Run",
  // Claude Code's own tools.
  Read: "Read",
  Edit: "Edit",
  MultiEdit: "Edit",
  Write: "Write",
  Bash: "Bash",
  PowerShell: "Run",
  Grep: "Search",
  Glob: "Find",
  WebFetch: "Fetch",
  WebSearch: "Search web",
  Task: "Agent",
  TodoWrite: "Tasks",
  NotebookEdit: "Notebook",
};

function stepIcon(state: WorkStep["state"]): Node {
  switch (state) {
    case "running":
      return h("i", { class: "step-spin" });
    case "done":
      return svg(ICONS.check, 11);
    case "failed":
      return svg(ICONS.bang, 11);
    case "declined":
      return svg(ICONS.xmark, 10);
    case "waiting":
      return h("i", { class: "step-wait" });
    default:
      return h("i", { class: "step-dash" });
  }
}

function stepRow(step: WorkStep): HTMLElement {
  // MCP tools arrive as mcp__server__tool: the tool's own name says enough.
  const verb = VERBS[step.tool] ?? (step.tool.startsWith("mcp__") ? (step.tool.split("__").pop() ?? step.tool) : step.tool);
  return h(
    "div",
    { class: `step ${step.state}` },
    h("span", { class: "step-icon" }, stepIcon(step.state)),
    h("span", { class: "step-verb", text: verb }),
    step.target ? h("span", { class: "step-target", dir: "auto", text: step.target }) : null,
  );
}

/** A coloured badge with the file's kind, like an editor tab. */
function fileBadge(file: string): HTMLElement {
  const ext = (file.split(".").pop() ?? "").toLowerCase();
  const kinds: Record<string, [string, string]> = {
    xlsx: ["XLS", "#22a35a"], xlsm: ["XLS", "#22a35a"], csv: ["CSV", "#22a35a"], ods: ["ODS", "#22a35a"],
    docx: ["DOC", "#3b82f6"], odt: ["ODT", "#3b82f6"], md: ["MD", "#9ca3af"], txt: ["TXT", "#9ca3af"],
    ts: ["TS", "#3178c6"], tsx: ["TSX", "#3178c6"], js: ["JS", "#e8c547"], rs: ["RS", "#dea584"],
    py: ["PY", "#4b8bbe"], json: ["{}", "#9ca3af"], html: ["<>", "#e34c26"], css: ["CSS", "#7c5cff"],
    sh: ["$", "#9ca3af"],
  };
  const [label, color] = file === "Terminal" ? ["$", "#4b5563"] : (kinds[ext] ?? [ext ? ext.slice(0, 3).toUpperCase() : "•", "#6b7280"]);
  const badge = h("span", { class: "file-badge", text: label });
  badge.style.background = color;
  return badge;
}

function sign(mark: DiffLine["mark"]): string {
  return mark === "removed" ? "-" : mark === "added" ? "+" : " ";
}

function renderText(lines: DiffLine[]): HTMLElement {
  const body = h("div", { class: "diff" });
  for (const line of lines) {
    if (line.mark === "gap") {
      body.append(h("div", { class: "diff-line gap", text: line.text || "⋯" }));
      continue;
    }
    const number = line.mark === "removed" ? line.old : (line.new ?? line.old);
    body.append(
      h(
        "div",
        { class: `diff-line ${line.mark}` },
        h("span", { class: "ln", text: number != null ? String(number) : "" }),
        h("span", { class: "sg", text: sign(line.mark) }),
        h("span", { class: "tx", dir: "auto", text: line.text || " " }),
      ),
    );
  }
  return body;
}

function renderTable(preview: Extract<Preview, { kind: "table" }>): HTMLElement {
  const table = h("table", { class: "sheet" });
  const head = h("tr", {}, h("th", { class: "corner", text: preview.sheet }));
  for (const c of preview.columns) head.append(h("th", { class: c.mark, text: c.label }));
  table.append(h("thead", {}, head));
  const body = h("tbody");
  for (const row of preview.rows) {
    if (row.mark === "gap") {
      body.append(h("tr", { class: "gap" }, h("td", { colspan: preview.columns.length + 1, text: "⋯" })));
      continue;
    }
    const tr = h("tr", { class: row.mark }, h("th", { text: row.label }));
    for (const cell of row.cells) {
      const td = h("td", { class: cell.mark, dir: "auto" });
      if (cell.old != null) td.append(h("s", { text: cell.old }), " ");
      td.append(h("span", { text: cell.text }));
      tr.append(td);
    }
    body.append(tr);
  }
  table.append(body);
  return h("div", { class: "sheet-wrap" }, table);
}

function renderDoc(preview: Extract<Preview, { kind: "doc" }>): HTMLElement {
  const body = h("div", { class: "doc" });
  for (const block of preview.blocks) {
    if (block.mark === "gap") {
      body.append(h("div", { class: "doc-block gap", text: "⋯" }));
      continue;
    }
    const text = h("div", { class: "doc-text", dir: "auto" });
    if (block.old != null) text.append(h("div", { class: "doc-old", text: block.old }));
    text.append(h("div", { text: block.text || " " }));
    body.append(h("div", { class: `doc-block ${block.mark}` }, h("span", { class: "doc-label", text: block.label }), text));
  }
  return body;
}

function renderPreview(preview: Preview): HTMLElement {
  switch (preview.kind) {
    case "text":
      return renderText(preview.lines);
    case "table":
      return renderTable(preview);
    case "doc":
      return renderDoc(preview);
  }
}

/** The key that says whether the panel needs drawing again. */
function panelKey(panel: WorkPanel | null): string {
  if (!panel) return "";
  return JSON.stringify([panel.file, panel.path, panel.approvalId, panel.hookRequestId, panel.outcome, panel.preview]);
}

export function buildWork(actions: ViewActions): ViewHost {
  const name = h("div", { class: "work-name", dir: "auto" });
  const sub = h("div", { class: "work-sub" });
  const steps = h("div", { class: "work-steps" });
  const side = h("div", { class: "work-side" }, name, sub, steps);

  const badge = h("span");
  const tabName = h("span", { class: "tab-name", dir: "ltr" });
  const tabDot = h("i", { class: "tab-dot" });
  const tab = h("div", { class: "work-tab" }, badge, tabName, tabDot);
  const path = h("span", { class: "work-path" });
  const status = h("span", { class: "work-status" });
  const top = h("div", { class: "work-top" }, tab, h("span", { class: "work-top-right" }, status, path));
  const body = h("div", { class: "work-body" });
  const stop = h("button", { class: "btn secondary" }, h("span", { text: "Stop" }));
  const deny = h("button", { class: "btn secondary" }, h("span", { text: "Deny" }));
  const allow = h("button", { class: "btn primary" }, h("span", { text: "Allow" }));
  const note = h("span", { class: "work-note", dir: "auto" });
  const foot = h("div", { class: "work-foot" }, note, h("span", { class: "actions" }, stop, deny, allow));
  const panel = h("div", { class: "work-panel" }, top, body, foot);

  const decide = (yes: boolean) => {
    const current = State.work?.panel;
    if (!current) return;
    if (current.approvalId != null) {
      const id = current.approvalId;
      current.approvalId = undefined;
      State.isPinned = false;
      void Bridge.toolDecision(id, yes);
    } else if (current.hookRequestId) {
      current.hookRequestId = undefined;
      actions.decide(yes ? "allow" : "deny");
      // decide() moves to the next view; the change itself stays on screen.
      actions.setView("work");
    }
    State.notify();
  };
  deny.addEventListener("click", () => decide(false));
  stop.addEventListener("click", () => {
    const step = State.work?.panel?.commandStep;
    if (step != null) void Bridge.stopCommand(step);
  });
  allow.addEventListener("click", () => decide(true));

  const el = h("div", { class: "view" }, h("div", { class: "card work-card" }, side, panel));

  let stepsKey = "";
  let drawn = "";
  return {
    el,
    sync() {
      const work = State.work;
      name.textContent = work?.who ?? "";
      sub.textContent = work?.sub ?? "";

      const shown = (work?.steps ?? []).slice(-MAX_STEPS);
      const key = JSON.stringify([shown, work?.active]);
      if (key !== stepsKey) {
        stepsKey = key;
        clear(steps);
        for (const s of shown) steps.append(stepRow(s));
        if (work && !work.active && shown.length) {
          steps.append(stepRow({ id: 0, tool: "Done", target: "", state: "done" }));
        }
      }

      const p = work?.panel ?? null;
      const waiting = !!p && (p.approvalId != null || !!p.hookRequestId);
      const running =
        !waiting && p?.commandStep != null && !!work?.steps.some((s) => s.id === p.commandStep && s.state === "running");
      foot.style.display = waiting || running ? "" : "none";
      foot.classList.toggle("running", running);
      stop.style.display = running ? "" : "none";
      deny.style.display = allow.style.display = waiting ? "" : "none";
      note.textContent = waiting ? (p?.note ?? "Make this change?") : running ? "Running…" : "";
      tabDot.classList.toggle("on", waiting);
      const command = p?.commandStep != null;
      const stepFailed = work?.steps.some((s) => s.id === p?.stepId && s.state === "failed");
      status.textContent = running
        ? ""
        : command && stepFailed
          ? "Did not finish"
        : p?.outcome === "applied" ? (command ? "Ran" : "Applied") : p?.outcome === "declined" ? (command ? "Not run" : "Declined") : "";
      status.className = `work-status ${command && stepFailed ? "declined" : (p?.outcome ?? "")}`;
      tabName.textContent = p?.file ?? "";
      tab.style.visibility = p ? "visible" : "hidden";
      // Marked left to right, or the right-to-left trick that keeps the end of a
      // long path visible would move its "~/" to the end.
      path.textContent = p ? `\u200E${p.path}\u200E` : "";

      const k = panelKey(p);
      if (k !== drawn) {
        drawn = k;
        clear(body);
        if (p) {
          clear(badge);
          badge.append(fileBadge(p.file));
          body.append(renderPreview(p.preview));
          // Bring the first change into view.
          requestAnimationFrame(() => {
            if (p.commandStep != null) {
              body.scrollTop = body.scrollHeight; // a terminal follows its end
              return;
            }
            const first = body.querySelector(".removed, .added, .changed") as HTMLElement | null;
            const above = first ? first.getBoundingClientRect().top - body.getBoundingClientRect().top : 0;
            body.scrollTop = Math.max(0, above - 44);
          });
        } else {
          body.append(h("div", { class: "work-empty", text: work?.active ? "Working…" : "" }));
        }
      }
    },
  };
}
