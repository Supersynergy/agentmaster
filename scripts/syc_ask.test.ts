import { expect, test } from "bun:test";
import { Readable } from "node:stream";
import { answerFromStream } from "./syc_ask";

function stream(...events: object[]) {
  return Readable.from(events.map(event => "data: " + JSON.stringify(event) + "\r\n\r\n"));
}

test("prints final answer parts once, including Unicode and line breaks", async () => {
  const event = { type: "response.completed", response: { status: "completed", output: [
    { type: "reasoning" },
    { type: "message", phase: "commentary", content: [{ type: "output_text", text: "Working..." }] },
    { type: "message", phase: "final_answer", content: [{ type: "output_text", text: "Grüße\n" }, { type: "output_text", text: "42" }] },
  ] } };
  const data = Buffer.from("event: response.completed\r\ndata: " + JSON.stringify(event) + "\r\n\r\n");
  const split = data.indexOf(Buffer.from("ü")) + 1;
  expect(await answerFromStream(Readable.from([data.subarray(0, split), data.subarray(split)]))).toBe("Grüße\n42");
});

test("an unfinished stream cannot report a successful partial answer", async () => {
  await expect(answerFromStream(stream({ type: "response.output_text.delta", delta: "partial" }))).rejects.toThrow("before response.completed");
  for (const type of ["response.failed", "response.incomplete", "error"]) {
    await expect(answerFromStream(stream({ type }))).rejects.toThrow(type);
  }
});

test("completed empty responses fail and provider refusals remain visible", async () => {
  const completed = (content: object[]) => stream({ type: "response.completed", response: {
    status: "completed", output: [{ type: "message", content }],
  } });
  await expect(answerFromStream(completed([]))).rejects.toThrow("no answer");
  expect(await answerFromStream(completed([{ type: "refusal", refusal: "Declined" }]))).toBe("Declined");
});
