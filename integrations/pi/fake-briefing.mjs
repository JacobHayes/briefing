#!/usr/bin/env node
// Stand-in for the `briefing` CLI in smoke-test.mjs. `present`/`demo` create `fixture-id`,
// taking BRIEFING_TEST_CREATE_MS to do it (as starting the hub can). BRIEFING_TEST_STATUS picks
// what `await` returns: completed (default), cancelled, failed, or wait (block until `cancel`
// records the id in BRIEFING_TEST_DIR, or until a signal stops the wait).
import { appendFileSync, readFileSync } from "node:fs";
import { join } from "node:path";

const [command, id] = process.argv.slice(2);
const cancelled = () => join(process.env.BRIEFING_TEST_DIR, "cancelled");
const url = "http://fixture.invalid/briefing/fixture-id";

if (command === "schema") {
  console.log(JSON.stringify({ type: "object", properties: {}, additionalProperties: false }));
} else if (command === "guidance") {
  console.log(JSON.stringify(["Fixture briefing guidance"]));
} else if (command === "present" || command === "demo") {
  process.stdin.resume();
  process.stdin.on("data", () => {});
  process.stdin.on("end", () =>
    setTimeout(() => {
      console.log(JSON.stringify({ briefingId: "fixture-id", status: "active", url }));
      process.exit(0);
    }, Number(process.env.BRIEFING_TEST_CREATE_MS || 0)),
  );
} else if (command === "cancel") {
  appendFileSync(cancelled(), `${id}\n`);
  console.log(JSON.stringify({ briefingId: id, cancelled: true }));
} else if (command === "await") {
  const status = process.env.BRIEFING_TEST_STATUS || "completed";
  const finish = (status) => {
    console.log(JSON.stringify({ briefingId: id, status, feedback: { questions: [], annotations: [], notes: [] } }));
    process.exit(0);
  };
  const isCancelled = () => {
    try {
      return readFileSync(cancelled(), "utf8").split("\n").includes(id);
    } catch {
      return false;
    }
  };
  // Handlers first: the test acts as soon as it sees the ready event.
  for (const signal of ["SIGINT", "SIGHUP"]) process.on(signal, () => process.exit(130));
  console.error(JSON.stringify({ event: "ready", briefingId: id, url }));
  if (status === "wait") {
    setInterval(() => isCancelled() && finish("cancelled"), 20);
  } else if (status === "failed") {
    console.error("fixture failure");
    process.exit(1);
  } else {
    finish(status);
  }
}
