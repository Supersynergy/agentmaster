// Embedded in the Rust CLI. SyCAdapter already runs on Bun.
import { once } from "node:events";
import { request, type IncomingMessage } from "node:http";
import { readFile, lstat, realpath } from "node:fs/promises";
import { resolve, relative, isAbsolute, sep } from "node:path";
import { createInterface } from "node:readline";
import { Readable } from "node:stream";

interface OutputItem {
  type: string;
  phase?: string;
  content?: { type: string; text?: string; refusal?: string }[];
}

interface ResponseEvent {
  type: string;
  response?: { status: string; usage?: Record<string, unknown>; output?: OutputItem[] };
}

export async function answerFromStream(input: Readable, receipt?: (usage?: Record<string, unknown>) => void): Promise<string> {
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
      receipt?.(event.response.usage);
      return text;
    }
    throw new Error("SyCAdapter stream ended before response.completed");
  } finally {
    lines.close();
  }
}

async function ask(model: string, question: string, mode: "ask" | "review" | "edit" = "ask", receipt?: (usage?: Record<string, unknown>) => void): Promise<string> {
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
      model, stream: true, store: false, instructions: mode === "edit" ? "Propose a change to the supplied file. The host applies it and runs the oracle. Return only JSON {file_path,edits:[{old_text,new_text}]} with unique exact preimages." : mode === "review" ? 'Review the supplied candidate against its contract. Return only JSON with verdict "pass" or "fail" and a concise reason identifying concrete behavior.' : "Answer the user's question.",
      ...(mode !== "ask" ? { reasoning: { effort: "medium" } } : {}),
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
      let usage: Record<string, unknown> | undefined;
      const text = await answerFromStream(response, value => { usage = value; });
      receipt?.(usage);
      if (mode !== "review") return text;
      const result = JSON.parse(text);
      if (!["pass", "fail"].includes(result?.verdict) || typeof result.reason !== "string" || !result.reason.trim())
        throw new Error("Invalid review verdict");
      return JSON.stringify({ verdict: result.verdict, reason: result.reason, usage });
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

async function singleEdit(model: string, file: string, taskFile: string, oracle: string[]) {
  if (!file || !taskFile || !oracle.length) throw new Error("Missing file, task or oracle");
  const cwd = await realpath(process.cwd()), target = resolve(cwd, file), rel = relative(cwd, target);
  if (!rel || rel === ".." || rel.startsWith(".." + sep) || isAbsolute(rel) || await realpath(target) !== target)
    throw new Error("Expected a workspace file without symlinks");
  const st = await lstat(target);
  if (!st.isFile() || st.nlink !== 1 || st.size > 128 * 1024) throw new Error("Expected a bounded, single-link file");
  const bytes = await readFile(target), source = bytes.toString("utf8");
  if (bytes.length > 128 * 1024 || source.includes("\0") || !bytes.equals(Buffer.from(source))) throw new Error("Expected bounded UTF-8 source");
  const task = (await readFile(taskFile, "utf8")).trim();
  if (!task) throw new Error("Empty task file");
  const editor = Bun.which("codex-one-edit");
  if (!editor) throw new Error("codex-one-edit is required for exact application and rollback");
  const { applyProposal } = await import(editor);
  if (typeof applyProposal !== "function") throw new Error("Installed editor lacks applyProposal");
  let usage: Record<string, unknown> | undefined;
  const answer = await ask(model, "Edit only " + file + ". " + task + "\nCurrent complete file:\n" + source, "edit", value => { usage = value; });
  applyProposal(file, source, JSON.parse(answer), oracle, cwd);
  console.log(JSON.stringify({ passed: true, file, usage }));
}

if (import.meta.main) {
  const [model, question, mode, file, taskFile, ...oracle] = process.argv.slice(1);
  const review = mode === "review-stdin";
  try {
    if (mode === "edit-file") { await singleEdit(model, file, taskFile, oracle); }
    else {
    const answer = await ask(model, review ? await Bun.stdin.text() : question, review ? "review" : "ask");
    console.log(answer);
    if (review && JSON.parse(answer).verdict === "fail") process.exitCode = 1;
    }
  } catch (err) {
    console.error(mode ? "agentmaster auto:" : "agentmaster ask:", err instanceof Error ? err.message : "request failed");
    process.exitCode = review ? 2 : 1;
  }
}
