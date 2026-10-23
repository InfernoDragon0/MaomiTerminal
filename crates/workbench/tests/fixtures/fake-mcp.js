// Stand-in for maomi-board-mcp.js in the process tests.
// usage: node fake-mcp.js <log file>
// Appends "spawn <pid>" on start and one JSON line per tools/call to the log file.
const fs = require("fs");
const log = process.argv[2];
const note = (line) => log && fs.appendFileSync(log, line + "\n");
const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
const text = (t, isError) => ({ content: [{ type: "text", text: t }], ...(isError ? { isError: true } : {}) });

note("spawn " + process.pid);
process.stderr.write("fake server started\n");

function handle(msg) {
  if (msg.method === "initialize") {
    note("initialize " + JSON.stringify(msg.params.clientInfo));
    return send({ jsonrpc: "2.0", id: msg.id, result: { protocolVersion: msg.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "fake", version: "0" } } });
  }
  if (msg.method === "notifications/initialized") return note("initialized");
  if (msg.method !== "tools/call") return;
  const { name, arguments: args } = msg.params;
  note("call " + JSON.stringify({ name, arguments: args }));
  // A stray notification before the reply must be skipped by the client.
  send({ jsonrpc: "2.0", method: "notifications/message", params: {} });
  switch (name) {
    case "hang": return;
    case "exit": return process.exit(0);
    case "pid": return send({ jsonrpc: "2.0", id: msg.id, result: text(String(process.pid)) });
    case "conflict": return send({ jsonrpc: "2.0", id: msg.id, result: text("Plan p-1 changed since your base_rev; it is now rev 9. Re-read and retry.", true) });
    case "tool_error": return send({ jsonrpc: "2.0", id: msg.id, result: text("card m-1 has no open review", true) });
    case "rpc_error": return send({ jsonrpc: "2.0", id: msg.id, error: { code: -32602, message: "Unknown tool: rpc_error" } });
    default: return send({ jsonrpc: "2.0", id: msg.id, result: text("ok " + name) });
  }
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl);
    buffer = buffer.slice(nl + 1);
    if (line.trim()) handle(JSON.parse(line));
  }
});
process.stdin.on("end", () => process.exit(0));
