// Capture the README screenshots and walkthrough GIF from docs/screenshots/showcase.json:
// `mise run docs:screenshots` (builds the debug binary first). Writes docs/screenshots/;
// the GIF needs ffmpeg on PATH, which the mise task provides. `node capture.mjs stills` or
// `node capture.mjs tour` runs one half.

import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

import { serve, stopAll } from "./serve.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const OUT = join(here, "../../docs/screenshots");
const SHOWCASE = join(OUT, "showcase.json");
// Stills sit two to a row in the README, so they use a narrow window that keeps the text
// legible at half width; the tour is shown full width.
const STILL = { width: 800, height: 560 };
// Wide enough for a comment to sit in the margin and the Outline to dock beside the page.
const WIDE = { width: 1280, height: 680 };
const TOUR = { width: 1200, height: 680 };
const GIF_WIDTH = 880;
const FPS = 12;

const COMMENTED = "Each lookup takes about 35 ms, and they run one after another.";
const COMMENT = "Could these run in parallel instead?";
const CHOICE = "Load price rules in one query";
const ANSWER = "Leave the tax code alone.";

let browser;

async function open(viewport, options = {}) {
  const context = await browser.newContext({ viewport, deviceScaleFactor: 2, ...options });
  const page = await context.newPage();
  await page.goto(await serve(SHOWCASE));
  await page.waitForSelector(".rich-vega-lite svg, .rich-vega-lite canvas");
  await settle(page);
  return page;
}

const settle = (page, ms = 400) => page.waitForTimeout(ms);
const header = (page, label) => page.locator(`header button:has-text("${label}")`);
const primary = page => page.locator(".nav .btn.primary");
const option = (page, label) => page.locator("label.option", { hasText: label }).first();

/** Where `selector` would sit with its top `offset` px below the sticky header. */
function scrollTarget(page, selector, offset = 24) {
  return page.locator(selector).first().evaluate((el, offset) => {
    const top = el.getBoundingClientRect().top + scrollY - document.querySelector("header").offsetHeight - offset;
    return Math.max(0, Math.min(top, document.documentElement.scrollHeight - innerHeight));
  }, offset);
}

/** Select `text` up to `length` characters; returns where the selection ends on screen. */
function selectText(page, text, length = text.length) {
  return page.evaluate(([text, length]) => {
    const walker = document.createTreeWalker(document.querySelector("main"), NodeFilter.SHOW_TEXT);
    for (let node; (node = walker.nextNode());) {
      const at = node.nodeValue.indexOf(text);
      if (at < 0) continue;
      const range = document.createRange();
      range.setStart(node, at);
      range.setEnd(node, at + length);
      getSelection().removeAllRanges();
      getSelection().addRange(range);
      document.dispatchEvent(new Event("selectionchange"));
      const rects = range.getClientRects();
      const last = rects[rects.length - 1] ?? range.getBoundingClientRect();
      return { x: last.right, y: last.top + last.height / 2 };
    }
    throw new Error("text not found: " + text);
  }, [text, length]);
}

async function stills() {
  const shot = (page, name) => page.screenshot({ path: join(OUT, name) });
  const jump = async (page, selector, offset) => {
    const y = await scrollTarget(page, selector, offset);
    await page.evaluate(y => scrollTo(0, y), y);
    await settle(page);
  };

  let page = await open(STILL);
  await jump(page, ".rich-vega-lite", 12);
  await shot(page, "chart.png");

  await page.evaluate(() => scrollTo(0, 0));
  await header(page, "Context").click();
  await settle(page);
  await page.evaluate(() => scrollTo(0, 0));
  await settle(page);
  await shot(page, "context.png");
  await header(page, "Hide context").click();

  await primary(page).click();
  await settle(page, 800);
  await jump(page, ".rich-mermaid", 12);
  await shot(page, "diagram.png");

  await primary(page).click();
  await settle(page, 800);
  await option(page, CHOICE).click();
  await jump(page, ".question", 4);
  await shot(page, "question.png");

  // The first press points at the question still open; the second goes on to the review.
  for (let i = 0; i < 3 && !(await page.$("button.btn.primary:has-text('Submit')")); i++) {
    await primary(page).click();
    await settle(page, 800);
  }
  await page.evaluate(() => scrollTo(0, 0));
  await settle(page);
  await shot(page, "review.png");
  await page.context().close();

  page = await open(WIDE);
  await primary(page).click();
  await settle(page, 800);
  await selectText(page, COMMENTED);
  await settle(page);
  await page.click(".comment-fab");
  await page.fill(".composer-input", COMMENT);
  await page.click(".composer .btn.primary");
  await page.evaluate(() => getSelection().removeAllRanges());
  await settle(page);
  await page.click(".anno-mark");
  await settle(page);
  await shot(page, "comment.png");
  await page.keyboard.press("Escape");

  await page.locator("#outline-toggle").click();
  await settle(page);
  await shot(page, "outline.png");
  await page.context().close();

  page = await open(STILL, { colorScheme: "dark" });
  await primary(page).click();
  await settle(page, 800);
  await jump(page, ".rich-mermaid", 12);
  await shot(page, "dark.png");
  await page.context().close();
}

/** A walkthrough recorded frame by frame with a drawn pointer: read, comment on a passage,
 * answer the questions, review and submit. Moves are captured at FPS and
 * pauses as single long frames; ffmpeg then builds a palette-optimised GIF. */
async function tour() {
  const dir = mkdtempSync(join(tmpdir(), "briefing-tour-"));
  const frames = [];
  const page = await open(TOUR);
  await page.evaluate(() => {
    const pointer = document.createElement("div");
    pointer.id = "tour-pointer";
    pointer.innerHTML = `<svg width="30" height="30" viewBox="0 0 24 24"><path d="M5 2.5v17l4.6-4.3 2.9 6.6 2.7-1.2-2.9-6.5h6.2z" fill="#111" stroke="#fff" stroke-width="1.6" stroke-linejoin="round"/></svg>`;
    pointer.style.cssText = "position:fixed;left:0;top:0;z-index:2147483647;pointer-events:none;transform-origin:4px 2px;filter:drop-shadow(0 1px 2px rgba(0,0,0,.35))";
    document.body.append(pointer);
  });
  let at = { x: TOUR.width * 0.72, y: TOUR.height * 0.55 };
  const place = (pressed = false) => page.evaluate(([x, y, pressed]) => {
    document.getElementById("tour-pointer").style.transform = `translate(${x - 4}px, ${y - 2}px) scale(${pressed ? 0.82 : 1})`;
  }, [at.x, at.y, pressed]);
  await place();

  const snap = async (seconds = 1 / FPS) => {
    const file = join(dir, `frame-${String(frames.length).padStart(4, "0")}.png`);
    await page.screenshot({ path: file });
    frames.push({ file, seconds });
  };
  const hold = async seconds => { await settle(page, 250); await snap(seconds); };
  const ease = t => (t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2);

  const moveTo = async (x, y) => {
    const from = at;
    const steps = Math.max(3, Math.min(8, Math.round(Math.hypot(x - from.x, y - from.y) / 80)));
    for (let i = 1; i <= steps; i++) {
      at = { x: from.x + (x - from.x) * ease(i / steps), y: from.y + (y - from.y) * ease(i / steps) };
      await page.mouse.move(at.x, at.y);
      await place();
      await snap();
    }
  };
  const moveToElement = async locator => {
    const box = await locator.boundingBox();
    await moveTo(box.x + Math.min(box.width / 2, 120), box.y + box.height / 2);
  };
  const click = async (locator, after = 0.5) => {
    await moveToElement(locator);
    await place(true);
    await snap(0.12);
    await locator.click();
    await place();
    await hold(after);
  };
  const scroll = async (selector, offset) => {
    const to = selector === null ? 0 : await scrollTarget(page, selector, offset);
    const from = await page.evaluate(() => scrollY);
    const steps = Math.max(4, Math.min(7, Math.round(Math.abs(to - from) / 80)));
    for (let i = 1; i <= steps; i++) {
      await page.evaluate(y => scrollTo(0, y), from + (to - from) * ease(i / steps));
      await snap();
    }
  };
  const type = async (locator, text) => {
    await locator.focus();
    for (let i = 0; i < text.length; i += 4) {
      await page.keyboard.type(text.slice(i, i + 4));
      await snap();
    }
  };

  // Read a section, then move on.
  await hold(1.3);
  await click(primary(page), 0.9);

  // Comment on one sentence.
  const start = await selectText(page, COMMENTED, 0);
  await page.evaluate(() => getSelection().removeAllRanges());
  await moveTo(start.x, start.y);
  for (let length = 9; length < COMMENTED.length + 9; length += 9) {
    at = await selectText(page, COMMENTED, Math.min(length, COMMENTED.length));
    await place();
    await snap();
  }
  await hold(0.4);
  await click(page.locator(".comment-fab"), 0.3);
  await type(page.locator(".composer-input"), COMMENT);
  await hold(0.3);
  await click(page.locator(".composer .btn.primary"), 0.2);
  await page.evaluate(() => getSelection().removeAllRanges());
  await hold(1.1);
  await click(primary(page), 0.8);

  // Answer the questions under the section they depend on.
  await scroll(".question", 4);
  await hold(0.6);
  await click(option(page, CHOICE), 0.9);
  const freeform = page.locator("section.question").nth(1);
  await scroll("section.question >> nth=1", 4);
  await click(freeform.locator("textarea"), 0.1);
  await type(freeform.locator("textarea"), ANSWER);
  await hold(0.6);

  // Review what goes back, then send it.
  await click(primary(page), 1.5);
  await click(page.locator("button.btn.primary:has-text('Submit')"), 2);

  const list = join(dir, "frames.txt");
  // The concat demuxer ignores the last entry's duration unless the file is repeated.
  const entries = [...frames, frames.at(-1)].map(f => `file '${f.file}'\nduration ${f.seconds.toFixed(3)}`);
  writeFileSync(list, entries.join("\n") + "\n");
  try {
    execFileSync("ffmpeg", [
      "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", list, "-fps_mode", "vfr",
      "-vf", `scale=${GIF_WIDTH}:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=48:stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle`,
      "-loop", "0", join(OUT, "tour.gif"),
    ]);
  } catch (error) {
    console.error(`tour.gif not written (needs ffmpeg): ${error.message}`);
    process.exitCode = 1;
  } finally {
    if (process.env.KEEP_FRAMES) console.error(`frames kept in ${dir}`);
    else rmSync(dir, { recursive: true, force: true });
  }
}

const only = process.argv[2];
browser = await chromium.launch();
try {
  if (only !== "tour") await stills();
  if (only !== "stills") await tour();
} finally {
  await browser.close();
  stopAll();
}
