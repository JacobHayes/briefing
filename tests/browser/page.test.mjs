// Browser regression tests for the briefing page: comment anchoring, tables, drafts.
//
// Each test serves tests/browser/fixture.json from the debug binary (override with
// BRIEFING_BIN) on loopback, fully isolated from the user's config, hub and briefing store,
// and drives it with Playwright's Chromium. They assert behaviour, never wording.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

const here = dirname(fileURLToPath(import.meta.url));
const BINARY = process.env.BRIEFING_BIN || join(here, "../../target/debug/briefing");
const FIXTURE = join(here, "fixture.json");

let browser;
const servers = [];

before(async () => { browser = await chromium.launch(); });
after(async () => {
  await browser?.close();
  for (const server of servers) server.stop();
});

/** Serve the fixture as a fresh briefing and return its page URL. `present` returns at once;
 * the briefing lives in the on-demand hub it starts in this test's own state dir, on a free
 * port, which is stopped once the tests finish. */
async function serve() {
  const dir = mkdtempSync(join(tmpdir(), "briefing-browser-"));
  const config = join(dir, "config.toml");
  writeFileSync(config, "");
  const state = join(dir, "briefings");
  const env = { ...process.env, BRIEFING_CONFIG: config, XDG_STATE_HOME: join(dir, "state"), BRIEFING_STATE_DIR: state, BRIEFING_PORT: "0" };
  for (const key of Object.keys(env)) if (key.startsWith("BRIEFING_") && !["BRIEFING_CONFIG", "BRIEFING_STATE_DIR", "BRIEFING_PORT"].includes(key)) delete env[key];
  servers.push({
    stop: () => {
      try { process.kill(JSON.parse(readFileSync(join(state, ".hub.json"), "utf8")).pid, "SIGKILL"); } catch { /* never started */ }
      rmSync(dir, { recursive: true, force: true });
    },
  });
  const stdout = await new Promise((resolve, reject) => {
    const child = spawn(BINARY, ["present", FIXTURE, "--json", "--bind", "local", "--open", "false"], { env, stdio: ["ignore", "pipe", "inherit"] });
    let out = "";
    child.stdout.on("data", chunk => (out += chunk));
    child.on("error", reject);
    child.on("exit", code => (code === 0 ? resolve(out) : reject(new Error(`briefing present exited with ${code}`))));
  });
  const { url } = JSON.parse(stdout);
  assert.match(url, /^http:\/\/127\.0\.0\.1:/, "tests must never reach a real hub");
  return url;
}

async function open(url, options = {}) {
  const context = await browser.newContext({ viewport: { width: 1100, height: 700 }, ...options });
  const page = await context.newPage();
  page.errors = [];
  page.on("pageerror", error => page.errors.push(error.message));
  await page.goto(url);
  await page.waitForSelector(".markdown table");
  await page.waitForTimeout(300);
  return page;
}

/** Select from the start of `from` to the end of `to` (text inside <main> unless `root` says otherwise). */
function select(page, from, to = from, root = "main") {
  return page.evaluate(([from, to, root]) => {
    const find = text => {
      const walker = document.createTreeWalker(document.querySelector(root), NodeFilter.SHOW_TEXT);
      for (let node; (node = walker.nextNode());) { const at = node.nodeValue.indexOf(text); if (at >= 0) return [node, at]; }
      throw new Error("text not found: " + text);
    };
    const [start, startAt] = find(from);
    const [end, endAt] = find(to);
    const range = document.createRange();
    range.setStart(start, startAt);
    range.setEnd(end, endAt + to.length);
    getSelection().removeAllRanges();
    getSelection().addRange(range);
    document.dispatchEvent(new Event("selectionchange"));
  }, [from, to, root]);
}

const commentButtonVisible = page => page.evaluate(() => !document.querySelector(".comment-fab").hidden);

async function comment(page, text) {
  await page.waitForTimeout(250);
  assert.ok(await commentButtonVisible(page), "the Comment action should appear for this selection");
  const [x, y] = await page.evaluate(() => { const r = document.querySelector(".comment-fab").getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; });
  await page.mouse.click(x, y);
  await page.fill(".composer-input", text);
  await page.click(".composer .btn.primary");
  await page.waitForTimeout(200);
}

async function done(page) {
  assert.deepEqual(page.errors, [], "the page threw");
  await page.context().close();
}

const highlighted = page => page.$$eval(".anno-mark", marks => marks.map(mark => mark.textContent).join(""));

test("a selection that runs onto the next paragraph's start still anchors a comment", async () => {
  const page = await open(await serve());
  // Triple-click style: from the start of one paragraph to offset 0 of the next.
  await page.evaluate(() => {
    const paragraphs = [...document.querySelectorAll(".detail p")];
    const first = paragraphs.find(p => p.textContent.startsWith("First paragraph"));
    const range = document.createRange();
    range.setStart(first.firstChild, 0);
    range.setEnd(first.nextElementSibling, 0);
    getSelection().removeAllRanges();
    getSelection().addRange(range);
    document.dispatchEvent(new Event("selectionchange"));
  });
  await comment(page, "whole paragraph");
  assert.equal(await highlighted(page), "First paragraph of the details, long enough to select on its own.");
  await done(page);
});

test("a selection spanning two regions offers no Comment action", async () => {
  const page = await open(await serve());
  await page.click("text=Context");
  await page.evaluate(() => {
    const walker = document.createTreeWalker(document.getElementById("context-panel"), NodeFilter.SHOW_TEXT);
    let start;
    for (let node; (node = walker.nextNode());) if (node.nodeValue.includes("Context panel text")) { start = node; break; }
    const end = document.querySelector("main h1").firstChild;
    const range = document.createRange();
    range.setStart(start, 0);
    range.setEnd(end, end.length);
    getSelection().removeAllRanges();
    getSelection().addRange(range);
    document.dispatchEvent(new Event("selectionchange"));
  });
  await page.waitForTimeout(250);
  assert.equal(await commentButtonVisible(page), false);
  await done(page);
});

test("a comment spanning table cells leaves every row with its own cells", async () => {
  const page = await open(await serve());
  await select(page, "beta one", "alpha two");
  await comment(page, "across cells");
  const rows = await page.$$eval(".detail table tr", rows => rows.map(row => [...row.children].map(cell => cell.tagName).join(",")));
  assert.deepEqual(rows, ["TH,TH,TH", "TD,TD,TD", "TD,TD,TD", "TD,TD,TD"]);
  assert.match(await highlighted(page), /beta one[\s\S]*alpha two/);
  await done(page);
});

test("highlights re-anchor after a reload", async () => {
  const url = await serve();
  const page = await open(url);
  await select(page, "Second paragraph");
  await comment(page, "persisted");
  await page.waitForTimeout(800); // debounced draft save
  await page.reload();
  await page.waitForSelector(".anno-mark");
  assert.equal(await highlighted(page), "Second paragraph");
  await done(page);
});

test("the Comment action stays with its selection while the page scrolls", async () => {
  const page = await open(await serve());
  await page.evaluate(() => scrollTo(0, 400));
  await select(page, "Filler 3");
  await page.waitForTimeout(250);
  const gap = () => page.evaluate(() => document.querySelector(".comment-fab").getBoundingClientRect().top - getSelection().getRangeAt(0).getBoundingClientRect().bottom);
  const before = await gap();
  await page.mouse.wheel(0, 200);
  await page.waitForTimeout(300);
  assert.ok(Math.abs((await gap()) - before) < 2, "the button moved relative to the selection");
  await done(page);
});

test("a half-typed note survives a reload", async () => {
  const page = await open(await serve());
  await page.click("#notes-toggle");
  await page.fill("#note-composer", "not yet added");
  await page.waitForTimeout(800);
  await page.reload();
  await page.waitForSelector(".markdown table");
  if (!(await page.$("#note-composer"))) await page.click("#notes-toggle");
  assert.equal(await page.inputValue("#note-composer"), "not yet added");
  await done(page);
});

test("notes added on two devices at once both survive", async () => {
  const url = await serve();
  const [a, b] = [await open(url), await open(url)];
  for (const [page, text] of [[a, "from A"], [b, "from B"]]) {
    await page.click("#notes-toggle");
    await page.fill("#note-composer", text);
    await page.click(".notes-composer .btn.primary");
  }
  await a.waitForTimeout(1500);
  const c = await open(url);
  await c.click("#notes-toggle");
  const notes = await c.$$eval(".note-item .markdown", nodes => nodes.map(node => node.textContent.trim()).sort());
  assert.deepEqual(notes, ["from A", "from B"]);
  for (const page of [a, b, c]) await done(page);
});

/** Move to the given screen index with the Back/Next buttons' own state (no reveal detours). */
async function goToScreen(page, index) {
  for (let step = 0; step < 10; step++) {
    const label = await page.textContent(".progress-step");
    if (label === `Step ${index + 1} of 2` || (index === 2 && label === "Review")) return;
    await page.click(".nav .btn.primary");
    await page.waitForTimeout(250);
  }
  throw new Error("could not reach screen " + index);
}

test("Next first shows an unanswered question that was never on screen", async () => {
  const page = await open(await serve(), { viewport: { width: 1100, height: 500 } });
  await goToScreen(page, 1);
  await page.evaluate(() => scrollTo(0, 0));
  await page.waitForTimeout(200);
  const hidden = await page.evaluate(() => document.querySelector('[data-question="c1-1"] h2').getBoundingClientRect().top > innerHeight);
  assert.ok(hidden, "the fixture's second question should start below the fold");
  await page.click(".nav .btn.primary");
  await page.waitForTimeout(800);
  assert.equal(await page.textContent(".progress-step"), "Step 2 of 2", "Next moved on without showing the question");
  const revealed = await page.evaluate(() => {
    const question = document.querySelector("[data-question].attention");
    const r = question?.querySelector("h2").getBoundingClientRect();
    return question && { key: question.dataset.question, onScreen: r.top >= 0 && r.bottom <= innerHeight };
  });
  assert.deepEqual(revealed, { key: "c1-1", onScreen: true });
  await done(page);
});

test("a chosen option can be cleared, and skipped questions go back as unresolved", async () => {
  const page = await open(await serve());
  await goToScreen(page, 1);
  const left = page.locator('[data-question="c1-0"] .option').first();
  await left.click();
  await left.click();
  assert.deepEqual(await page.$$eval('[data-question="c1-0"] input', boxes => boxes.map(box => box.checked)), [false, false]);
  await page.fill('[data-question="c1-1"] .question-answer', "my own words");
  await goToScreen(page, 2);
  // As the page itself asks: API routes need its protocol header.
  const { keptFor } = await page.evaluate(async () => (await fetch(location.pathname.replace("/briefing/", "/api/") + "/presentation", { headers: { "briefing-protocol": PROTOCOL } })).json());
  const submitted = page.waitForRequest(request => request.url().endsWith("/complete"));
  for (let i = 0; i < 4 && !(await page.$(".done")); i++) {
    await page.click(".nav .btn.primary");
    await page.waitForTimeout(400);
  }
  const questions = JSON.parse((await submitted).postData()).questions;
  assert.deepEqual(questions.map(q => [q.question, q.selected, q.answer]), [
    ["Pick a direction?", [], ""],
    ["Anything to add?", [], "my own words"],
    ["Ship it?", [], ""],
  ]);
  assert.equal(questions[0].section, "Second");
  assert.equal(questions[2].section, undefined);
  // The Submitted screen says how long the hub keeps the result, from the hub's own TTL.
  assert.ok(keptFor, "the page payload carries the retention");
  assert.ok((await page.textContent(".done")).includes(keptFor), "the Submitted screen shows the hub's retention");
  await done(page);
});

test("the Outline jumps to a section, and switching tabs keeps unsent note text", async () => {
  // Wide enough for the docked sidebar, which stays open after a jump.
  const page = await open(await serve(), { viewport: { width: 1400, height: 800 } });
  await page.click("#notes-toggle");
  await page.fill("#note-composer", "unsent");
  await page.click('[data-tab="outline"]');
  await page.click(".outline-item >> nth=1");
  await page.waitForTimeout(300);
  assert.equal(await page.textContent(".progress-step"), "Step 2 of 2");
  await page.click('[data-tab="notes"]');
  assert.equal(await page.inputValue("#note-composer"), "unsent");
  await done(page);
});

test("every load starts with the sidebar closed, whatever an earlier load left open", async () => {
  // A real hub serves every briefing from one origin; here each server has its own, so a second
  // load of the same briefing (same origin, same browser context) stands in for a fresh one.
  const url = await serve();
  const page = await open(url, { viewport: { width: 1400, height: 800 } });
  await page.click('[data-tab="outline"], #outline-toggle');
  assert.ok(await page.$("#notes-panel"));
  const next = await page.context().newPage();
  await next.goto(url);
  await next.waitForSelector(".markdown table");
  assert.equal(await next.$("#notes-panel"), null);
  await next.close();
  await done(page);
});

test("a link to an earlier section jumps to it", async () => {
  const page = await open(await serve());
  await goToScreen(page, 1);
  await page.click('a[href="#section-1"]');
  await page.waitForTimeout(300);
  assert.equal(await page.textContent(".progress-step"), "Step 1 of 2");
  await done(page);
});

test("a key point with its own sub-list renders it as nested bullets", async () => {
  const page = await open(await serve());
  const points = await page.$$eval(".points > li", items => items.map(li => ({
    lead: li.querySelector("strong")?.textContent ?? li.textContent.trim(),
    nested: [...li.querySelectorAll(":scope > ul > li")].map(n => n.textContent.trim()),
  })));
  assert.deepEqual(points, [
    { lead: "A plain key point.", nested: [] },
    { lead: "Grouped findings:", nested: ["first nested finding", "second nested finding"] },
  ]);
  await done(page);
});
