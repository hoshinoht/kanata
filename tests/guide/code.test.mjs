import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { test } from "node:test";

const highlight = readFileSync(new URL("../../src/server/guide/highlight.js", import.meta.url), "utf8");
const examples = readFileSync(new URL("../../src/server/guide/examples.js", import.meta.url), "utf8");
class TextNode {
  children = [];
  text = "";
  append(...children) { this.children.push(...children); }
  set textContent(value) { this.text = value; }
  get textContent() { return this.text + this.children.map(child => child.textContent).join(""); }
}
const document = {
  createElement(tag) { assert.equal(tag, "span"); return new TextNode(); },
  createTextNode(value) { const node = new TextNode(); node.textContent = value; return node; },
};
const context = { document };
runInNewContext(highlight + "\n" + examples, context);

test("highlighting preserves source and treats HTML-looking model names as text", () => {
  for (const audio of [false, true]) {
    for (const example of context.requestExamples("https://gateway.example/v1", '</script><img src=x onerror="alert(1)">', audio)) {
      const code = new TextNode();
      context.highlightCode(code, example.source, example.language);
      assert.equal(code.textContent, example.source);
      assert.ok(code.children.some(node => node.className === "token-string"));
    }
  }
});

test("Rust quoting preserves literal backslashes and escapes control characters", () => {
  const example = context.requestExamples("https://gateway.example/v1", 'alias\\u1234\b\f\u0001"').find(x => x.language === "rust");
  assert.ok(example.source.includes('alias\\\\u1234\\u{8}\\u{c}\\u{0001}\\"'));
});

test("chat examples include a selected effort and audio examples omit it", () => {
  for (const example of context.requestExamples("https://gateway.example/v1", "family", false, "low")) {
    assert.match(example.source, /reasoning_effort/);
    assert.match(example.source, /low/);
  }
  for (const audio of [false, true]) {
    for (const example of context.requestExamples("https://gateway.example/v1", "ordinary", audio, audio ? "low" : undefined)) {
      assert.doesNotMatch(example.source, /reasoning_effort/);
    }
  }
});
