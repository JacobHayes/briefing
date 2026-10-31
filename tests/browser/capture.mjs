// Capture the README screenshots and walkthrough GIF from assets/demo.json:
// `mise run docs:screenshots` (builds the debug binary first). Writes docs/screenshots/;
// the GIF needs ffmpeg on PATH, which the mise task provides.

import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

import { serve, stopAll } from "./serve.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const DEMO = join(here, "../../assets/demo.json");
const OUT = join(here, "../../docs/screenshots");
const VIEWPORT = { width: 1280, height: 800 };
const COMMENT = "Could Next ever auto-advance once every question is answered?";

let browser;

async function open(options = {}) {
  const context = await browser.newContext({ viewport: VIEWPORT, ...options });
  const page = await context.newPage();
  await page.goto(await serve(DEMO));
  await page.waitForSelector(".rich-mermaid svg");
  await settle(page);
  return page;
}

const settle = (page, ms = 400) => page.waitForTimeout(ms);
const shot = (page, name) => page.screenshot({ path: join(OUT, name) });
const header = (page, label) => page.click(`header button:has-text("${label}")`);

async function centre(page, selector) {
  await page.locator(selector).first().evaluate(el => el.scrollIntoView({ block: "center" }));
  await settle(page);
}

async function next(page) {
  await page.click(".nav .btn.primary");
  await settle(page, 600);
}

/** Select a whole key point and leave a comment on it. */
async function comment(page, startsWith, text, frame = async () => {}) {
  await page.evaluate(startsWith => {
    const item = [...document.querySelectorAll(".detail li, main li")].find(li => li.textContent.startsWith(startsWith));
    const range = document.createRange();
    range.selectNodeContents(item);
    getSelection().removeAllRanges();
    getSelection().addRange(range);
    document.dispatchEvent(new Event("selectionchange"));
  }, startsWith);
  await settle(page);
  await frame();
  await page.click(".comment-fab");
  await page.fill(".composer-input", text);
  await settle(page, 200);
  await frame();
  await page.click(".composer .btn.primary");
  await page.evaluate(() => getSelection().removeAllRanges());
  await settle(page);
}

/** Click Next/Review until the review screen is up. */
async function toReview(page) {
  for (let i = 0; i < 8 && !(await page.$("button.btn.primary:has-text('Submit')")); i++) await next(page);
}

async function screenshots() {
  let page = await open();
  await comment(page, "Nothing advances", COMMENT);
  await page.click(".anno-mark");
  await settle(page);
  await shot(page, "03-inline-comment.png");
  await page.keyboard.press("Escape");

  await header(page, "Context");
  await settle(page);
  await shot(page, "04-context.png");
  await header(page, "Hide context");

  await header(page, "Outline");
  await settle(page);
  await shot(page, "07-outline.png");
  await page.click(".notes-close");

  await centre(page, ".rich-mermaid");
  await shot(page, "02-rich-content.png");

  await page.click("text=Paced, one chunk per screen");
  await centre(page, ".question");
  await shot(page, "05-question.png");

  await header(page, "Outline");
  await page.locator(".outline-item").nth(1).click();
  await page.click(".notes-close");
  await settle(page, 600);
  await centre(page, ".rich-vega-lite");
  await shot(page, "02b-chart.png");

  await toReview(page);
  await shot(page, "06-review.png");
  await page.context().close();

  page = await open({ colorScheme: "dark" });
  await centre(page, ".rich-mermaid");
  await shot(page, "08-dark.png");
  await page.context().close();
}

/** A walkthrough: read, comment, open the Context, answer a question, review. Each frame is
 * held for its own duration, then ffmpeg turns the frames into a palette-optimised GIF. */
async function tour() {
  const dir = mkdtempSync(join(tmpdir(), "briefing-tour-"));
  const frames = [];
  const page = await open();
  const frame = async (seconds = 1.2) => {
    const file = join(dir, `frame-${String(frames.length).padStart(3, "0")}.png`);
    await page.screenshot({ path: file });
    frames.push({ file, seconds });
  };
  await frame(2);
  await comment(page, "Nothing advances", COMMENT, () => frame(1.4));
  await frame(1.6);
  await header(page, "Context");
  await settle(page);
  await frame(2);
  await header(page, "Hide context");
  await centre(page, ".rich-mermaid");
  await frame(1.6);
  await centre(page, ".question");
  await frame(1);
  await page.click("text=Paced, one chunk per screen");
  await settle(page);
  await frame(1.6);
  await toReview(page);
  await frame(2.5);

  const list = join(dir, "frames.txt");
  // The concat demuxer ignores the last entry's duration unless the file is repeated.
  const entries = [...frames, frames.at(-1)].map(f => `file '${f.file}'\nduration ${f.seconds}`);
  writeFileSync(list, entries.join("\n") + "\n");
  try {
    execFileSync("ffmpeg", [
      "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", list,
      "-vf", "scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=96[p];[b][p]paletteuse=dither=none",
      "-loop", "0", join(OUT, "tour.gif"),
    ]);
  } catch (error) {
    console.error(`tour.gif not written (needs ffmpeg): ${error.message}`);
    process.exitCode = 1;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

browser = await chromium.launch();
try {
  await screenshots();
  await tour();
} finally {
  await browser.close();
  stopAll();
}
