// Demo: exercise every am-* tool once, print results. Useful for smoke-testing
// the adapter without a Cursor install.
import { amLs, amDoctor, amSkill, createAgentmasterTools } from "./index.js";

async function main() {
  console.log("=== am ls ===");
  console.log(await amLs({}));

  console.log("\n=== am doctor --costs ===");
  console.log(await amDoctor({ costs: true }));

  console.log("\n=== am skill claude ===");
  console.log(await amSkill({ runtime: "claude" }));

  console.log("\n=== registered tools ===");
  for (const t of createAgentmasterTools()) {
    console.log(`  ${t.name}: ${t.description.slice(0, 80)}…`);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
