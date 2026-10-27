function requestExamples(base, model, audio = false, effort) {
  const url = JSON.stringify(base + (audio ? "/audio/transcriptions" : "/chat/completions"));
  const alias = JSON.stringify(model);
  const reasoning = !audio && effort ? {reasoning_effort: effort} : {};
  const payload = JSON.stringify({model, ...reasoning, messages: [{role: "user", content: "Hello!"}]});
  const effortLine = reasoning.reasoning_effort ? `\n        "reasoning_effort": ${JSON.stringify(effort)},` : "";
  const rustString = (value) => value.replace(/\\(?:["\\/bfnrt]|u[0-9a-f]{4})/gi, (escape) => {
    if (escape.startsWith("\\u")) return `\\u{${escape.slice(2)}}`;
    if (escape === "\\b") return "\\u{8}";
    if (escape === "\\f") return "\\u{c}";
    return escape;
  });
  return [
    {
      label: "JavaScript", language: "javascript", filename: "request.mjs",
      setup: "Node.js 22+ · Save as request.mjs, then run node request.mjs. Uses built-in fetch.",
      source: `${audio ? 'import { readFile } from "node:fs/promises";\n\n' : ''}const key = process.env.KANATA_API_KEY;
if (!key) throw new Error("Set KANATA_API_KEY first");
${audio ? `const body = new FormData();
body.set("model", ${alias});
body.set("file", new Blob([await readFile("sample.wav")], {
  type: "audio/wav",
}), "sample.wav");` : `const body = JSON.stringify(${JSON.stringify(JSON.parse(payload), null, 2)});`}
const response = await fetch(${url}, {
  method: "POST",
  headers: {
    Authorization: "Bearer " + key,${audio ? '' : '\n    "Content-Type": "application/json",'}
  },
  body,
  signal: AbortSignal.timeout(60_000),
});
if (!response.ok) {
  throw new Error("HTTP " + response.status + ": " + await response.text());
}
console.log(await response.json());`,
    },
    {
      label: "Python", language: "python", filename: "request.py",
      setup: "Python 3 · Install requests with python -m pip install requests, then run python request.py.",
      source: `import os
import requests

headers = {"Authorization": "Bearer " + os.environ["KANATA_API_KEY"]}
${audio ? `with open("sample.wav", "rb") as audio:
    response = requests.post(
        ${url},
        headers=headers,
        data={"model": ${alias}},
        files={"file": ("sample.wav", audio, "audio/wav")},
        timeout=60,
    )` : `response = requests.post(
    ${url},
    headers=headers,
    json={
        "model": ${alias},${effortLine}
        "messages": [{"role": "user", "content": "Hello!"}],
    },
    timeout=60,
)`}
response.raise_for_status()
print(response.json())`,
    },
    {
      label: "Go", language: "go", filename: "request.go",
      setup: "Go · Standard library only. Save as request.go, then run go run request.go.",
      source: `package main

import (
${audio ? '\t"bytes"\n\t"mime/multipart"\n\t"net/textproto"' : '\t"strings"'}
\t"fmt"
\t"io"
\t"net/http"
\t"os"
\t"time"
)

func run() error {
\tkey := os.Getenv("KANATA_API_KEY")
\tif key == "" { return fmt.Errorf("set KANATA_API_KEY first") }
${audio ? `\tvar body bytes.Buffer
\twriter := multipart.NewWriter(&body)
\tif err := writer.WriteField("model", ${alias}); err != nil { return err }
\tfile, err := os.Open("sample.wav")
\tif err != nil { return err }
\tdefer file.Close()
\theader := make(textproto.MIMEHeader)
\theader.Set("Content-Disposition", "form-data; name=\\"file\\"; filename=\\"sample.wav\\"")
\theader.Set("Content-Type", "audio/wav")
\tpart, err := writer.CreatePart(header)
\tif err != nil { return err }
\tif _, err = io.Copy(part, file); err != nil { return err }
\tif err = writer.Close(); err != nil { return err }
\trequest, err := http.NewRequest("POST", ${url}, &body)` : `\tbody := strings.NewReader(${JSON.stringify(payload)})
\trequest, err := http.NewRequest("POST", ${url}, body)`}
\tif err != nil { return err }
\trequest.Header.Set("Authorization", "Bearer " + key)
\trequest.Header.Set("Content-Type", ${audio ? 'writer.FormDataContentType()' : '"application/json"'})
\tclient := &http.Client{Timeout: 60 * time.Second}
\tresponse, err := client.Do(request)
\tif err != nil { return err }
\tdefer response.Body.Close()
\tresult, err := io.ReadAll(response.Body)
\tif err != nil { return err }
\tif response.StatusCode >= 400 {
\t\treturn fmt.Errorf("HTTP %d: %s", response.StatusCode, result)
\t}
\tfmt.Println(string(result))
\treturn nil
}

func main() {
\tif err := run(); err != nil {
\t\tfmt.Fprintln(os.Stderr, err)
\t\tos.Exit(1)
\t}
}`,
    },
    {
      label: "Rust", language: "rust", filename: "src/main.rs",
      setup: 'Rust · In a Cargo project: cargo add reqwest@0.12 --features blocking,json,multipart; cargo add serde_json. Save as src/main.rs, then cargo run.',
      source: `${audio ? 'use reqwest::blocking::multipart::{Form, Part};' : 'use serde_json::json;'}
use std::{env, error::Error, time::Duration};

fn main() -> Result<(), Box<dyn Error>> {
    let key = env::var("KANATA_API_KEY")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
${audio ? `    let file = Part::file("sample.wav")?.mime_str("audio/wav")?;
    let body = Form::new()
        .text("model", ${rustString(alias)})
        .part("file", file);` : `    let body = json!({
        "model": ${rustString(alias)},${effortLine}
        "messages": [{"role": "user", "content": "Hello!"}],
    });`}
    let response = client.post(${rustString(url)})
        .bearer_auth(key)
        .${audio ? 'multipart(body)' : 'json(&body)'}
        .send()?
        .error_for_status()?
        .json::<serde_json::Value>()?;
    println!("{}", response);
    Ok(())
}`,
    },
  ];
}
