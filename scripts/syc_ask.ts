// Embedded in the Rust CLI. SyCAdapter already runs on Bun.
import { once } from "node:events";
import { request, type IncomingMessage } from "node:http";
import { readFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import { Readable } from "node:stream";

interface OutputItem {
  type: string;
  phase?: string;
  content?: { type: string; text?: string; refusal?: string }[];
}

interface ResponseEvent {
  type: string;
  response?: { status: string; output?: OutputItem[] };
}

export async function answerFromStream(input: Readable): Promise<string> {
  const lines = createInterface({ input, crlfDelay: Infinity });
  let data: string[] = [];
  try {
    for await (const line of lines) {
      if (line.startsWith("data:")) {
        data.push(line.slice(5).trimStart());
        continue;
      }
      if (line !== "" || !data.length) continue;
      const payload = data.join("\n");
      data = [];
      if (payload === "[DONE]") break;
      const event: ResponseEvent = JSON.parse(payload);
      if (["error", "response.failed", "response.incomplete"].includes(event.type)) {
        throw new Error("SyCAdapter response ended with " + event.type);
      }
      if (event.type !== "response.completed") continue;
      if (event.response?.status !== "completed") throw new Error("Response was not completed");
      const text = (event.response.output ?? [])
        .filter(item => item.type === "message" && item.phase !== "commentary")
        .flatMap(item => item.content ?? [])
        .map(part => part.type === "output_text" ? part.text : part.type === "refusal" ? part.refusal : "")
        .join("");
      if (!text.trim()) throw new Error("Completed response contained no answer");
      return text;
    }
    throw new Error("SyCAdapter stream ended before response.completed");
  } finally {
    lines.close();
  }
}

async function ask(model: string, question: string): Promise<string> {
  if (!model?.trim() || !question?.trim()) throw new Error("Provide a model and a non-empty question");
  const cancel = new AbortController();
  const interrupt = () => cancel.abort(new Error("Request interrupted"));
  process.once("SIGINT", interrupt);
  process.once("SIGTERM", interrupt);
  const signal = AbortSignal.any([cancel.signal, AbortSignal.timeout(180_000)]);
  const adapter = Bun.spawn(["sycadapter", "--model", model], {
    stdin: "ignore", stdout: "pipe", stderr: "inherit",
  });
  const startup = createInterface({ input: Readable.from(adapter.stdout), crlfDelay: Infinity });
  try {
    const [line] = await Promise.race([
      once(startup, "line", { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
      adapter.exited.then(code => { throw new Error("SyCAdapter exited with code " + code); }),
    ]);
    const ready: { baseURL: string; keyFile: string; model: string } = JSON.parse(line);
    const url = new URL(ready.baseURL);
    if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port ||
        url.pathname !== "/v1" || url.username || url.password || url.search || url.hash ||
        ready.model !== model) {
      throw new Error("Unexpected SyCAdapter endpoint or model");
    }
    url.pathname += "/responses";
    const token = (await readFile(ready.keyFile, "utf8")).trim();
    if (!/^[a-f0-9]{64}$/.test(token)) throw new Error("Invalid SyCAdapter key");
    const body = JSON.stringify({
      model, stream: true, store: false, instructions: "Answer the user's question.",
      input: [{ role: "user", content: [{ type: "input_text", text: question }] }],
    });
    const response = await new Promise<IncomingMessage>((resolve, reject) => {
      const req = request(url, { method: "POST", signal, agent: false, headers: {
        Authorization: "Bearer " + token, "Content-Type": "application/json", Accept: "text/event-stream",
      } }, resolve);
      req.on("error", reject);
      req.end(body);
    });
    try {
      if (response.statusCode !== 200) {
        const auth = [401, 403, 503].includes(response.statusCode ?? 0) ? " (check SynergyCoder login and model access)" : "";
        throw new Error("SyCAdapter HTTP " + response.statusCode + auth);
      }
      return await answerFromStream(response);
    } finally {
      response.destroy();
    }
  } finally {
    startup.close();
    adapter.kill("SIGTERM");
    const force = setTimeout(() => adapter.kill("SIGKILL"), 2_000);
    try { await adapter.exited; } finally {
      clearTimeout(force);
      process.removeListener("SIGINT", interrupt);
      process.removeListener("SIGTERM", interrupt);
    }
  }
}

if (import.meta.main) {
  try {
    const [model, question] = process.argv.slice(1);
    console.log(await ask(model, question));
  } catch (err) {
    console.error("agentmaster ask:", err instanceof Error ? err.message : "request failed");
    process.exitCode = 1;
  }
}
