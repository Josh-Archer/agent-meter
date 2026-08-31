import { CopilotClient } from "@github/copilot-sdk";
import { fileURLToPath } from "node:url";

// Pin the SDK to the runtime it installed beside this helper. This avoids
// relying on package subpath exports, which have changed between CLI releases.
const cliPath = fileURLToPath(
  new URL("./node_modules/.bin/copilot", import.meta.url),
);
const client = new CopilotClient({ connection: { kind: "stdio", path: cliPath } });
try {
  await client.start();
  const result = await client.rpc.account.getQuota({});
  // Quota fields only: never emit credentials, identity, or session data.
  process.stdout.write(`${JSON.stringify(result)}\n`);
} finally {
  await client.stop();
}
