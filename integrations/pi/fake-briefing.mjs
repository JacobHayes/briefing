#!/usr/bin/env node
// Stand-in for the `briefing` CLI in smoke-test.mjs. BRIEFING_TEST_STATUS picks the outcome:
// completed (default), cancelled, failed, or wait (block until SIGINT cancels or SIGHUP detaches).
const command = process.argv[2];
if (command === "schema") {
  console.log(JSON.stringify({ type: "object", properties: {}, additionalProperties: false }));
} else if (command === "guidance") {
  console.log(JSON.stringify(["Fixture briefing guidance"]));
} else {
  const id = command === "await" ? process.argv[3] : "fixture-id";
  const status = process.env.BRIEFING_TEST_STATUS || "completed";
  process.stdin.resume();
  process.stdin.on("data", () => {});
  const finish = (status) => {
    console.log(JSON.stringify({ briefingId: id, status, feedback: { questions: [], annotations: [], notes: [] } }));
    process.exit(status === "cancelled" ? 2 : 0);
  };
  const ready = () => console.error(JSON.stringify({ event: "ready", id, url: "http://fixture.invalid/briefing", scope: "local", label: "fixture", openedBrowser: false }));
  if (status === "wait") {
    // Handlers first: the test signals as soon as it sees the ready event.
    process.on("SIGINT", () => finish("cancelled"));
    process.on("SIGHUP", () => process.exit(130));
    ready();
    setInterval(() => {}, 1000);
  } else {
    ready();
    if (status === "failed") {
      console.error("fixture failure");
      process.exit(1);
    }
    finish(status);
  }
}
