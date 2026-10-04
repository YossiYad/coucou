// The frame drawn around a screen Coucou can see. The window itself is
// transparent and lets every click through; this only draws the glow, and
// pulses it when a screenshot is taken.

import "./frame.css";
import { onEvent } from "../core/bridge";

const frame = document.getElementById("frame");

void onEvent<null>("frame-pulse", () => {
  if (!frame) return;
  frame.classList.remove("pulse");
  // Restart the animation on every screenshot.
  void frame.offsetWidth;
  frame.classList.add("pulse");
});
