(() => {
  "use strict";

  const byId = (id) => document.getElementById(id);
  const base = `${location.origin}/v1`;
  const form = byId("key-form");
  const input = byId("api-key");
  const status = byId("status");
  const select = byId("model-select");
  const secure = location.protocol === "https:" || ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  let key = "";
  let models = [];
  let controller;
  let requestNumber = 0;
  class GuideError extends Error {}

  function element(tag, text, className) {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  }

  const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
  const authHeader = '  -H "Authorization: Bearer $KANATA_API_KEY"';
  function curl(path, body, streaming = false) {
    return `curl${streaming ? " -N" : ""} ${quote(base + path)} \\\n${authHeader} \\\n  -H 'Content-Type: application/json' \\\n  --data ${quote(JSON.stringify(body, null, 2))}`;
  }

  let exampleNumber = 0;
  let preferredLanguage = "shell";

  function examples(parent, title, snippets) {
    const group = element("section", undefined, "examples");
    group.setAttribute("aria-label", title);
    const toolbar = element("div", undefined, "code-toolbar");
    const tabs = element("div", undefined, "code-tabs");
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", `${title} language`);
    const copy = element("button", "Copy code", "copy");
    copy.type = "button";
    const panel = element("div", undefined, "code-panel");
    panel.id = `example-${++exampleNumber}`;
    panel.setAttribute("role", "tabpanel");
    panel.tabIndex = 0;
    const meta = element("div", undefined, "code-meta");
    const filename = element("span");
    const announcement = element("span");
    announcement.setAttribute("role", "status");
    meta.append(filename, announcement);
    const pre = element("pre");
    pre.tabIndex = 0;
    pre.setAttribute("aria-label", `${title} source code`);
    const code = element("code");
    pre.append(code);
    const setup = element("p", undefined, "code-setup");
    setup.id = `${panel.id}-setup`;
    panel.setAttribute("aria-describedby", setup.id);
    panel.append(meta, pre, setup);
    let active;
    let copyVersion = 0;
    const buttons = snippets.map((snippet, index) => {
      const button = element("button", snippet.label);
      button.id = `${panel.id}-tab-${index}`;
      button.type = "button";
      button.setAttribute("role", "tab");
      button.setAttribute("aria-controls", panel.id);
      button.addEventListener("click", () => activate(index));
      button.addEventListener("keydown", (event) => {
        let next;
        if (event.key === "ArrowRight") next = (index + 1) % snippets.length;
        if (event.key === "ArrowLeft") next = (index + snippets.length - 1) % snippets.length;
        if (event.key === "Home") next = 0;
        if (event.key === "End") next = snippets.length - 1;
        if (next === undefined) return;
        event.preventDefault();
        activate(next);
        buttons[next].focus();
      });
      tabs.append(button);
      return button;
    });
    function activate(index) {
      active = snippets[index];
      preferredLanguage = active.language;
      copyVersion += 1;
      announcement.textContent = "";
      copy.textContent = "Copy code";
      copy.setAttribute("aria-label", `Copy ${title} ${active.label} example`);
      buttons.forEach((button, i) => {
        button.setAttribute("aria-selected", String(i === index));
        button.tabIndex = i === index ? 0 : -1;
      });
      panel.setAttribute("aria-labelledby", buttons[index].id);
      filename.textContent = active.filename;
      setup.textContent = active.setup;
      code.replaceChildren();
      highlightCode(code, active.source, active.language);
      pre.scrollTop = 0;
      pre.scrollLeft = 0;
    }
    copy.addEventListener("click", async () => {
      const version = copyVersion;
      try {
        await navigator.clipboard.writeText(active.source);
        if (version === copyVersion) announcement.textContent = "Copied";
      } catch {
        if (version === copyVersion) announcement.textContent = "Select the code and copy it manually.";
      }
    });
    activate(Math.max(0, snippets.findIndex((snippet) => snippet.language === preferredLanguage)));
    toolbar.append(tabs, copy);
    group.append(toolbar, panel);
    parent.append(group);
  }

  function codeBlock(parent, title, source) {
    examples(parent, title, [{label: "cURL", language: "shell", filename: "Terminal", setup: "Set KANATA_API_KEY in your environment before running this request.", source}]);
  }

  function fields(parent, rows) {
    const table = element("table");
    const head = element("thead");
    const header = element("tr");
    for (const label of ["Field", "Contract"]) {
      const th = element("th", label);
      th.scope = "col";
      header.append(th);
    }
    head.append(header);
    const body = element("tbody");
    for (const [name, description] of rows) {
      const row = element("tr");
      const field = element("td");
      field.append(element("code", name));
      row.append(field, element("td", description));
      body.append(row);
    }
    table.append(head, body);
    parent.append(table);
  }

  function endpoint(path, title, description) {
    const panel = element("article", undefined, "panel");
    panel.append(element("h3", title));
    const heading = element("div", undefined, "endpoint");
    heading.append(element("span", "POST", "method"), element("code", `/v1${path}`));
    const body = element("div", undefined, "endpoint-body");
    const details = element("div", undefined, "endpoint-details");
    details.append(element("p", description, "muted"));
    body.append(details);
    panel.append(heading, body);
    return {panel, details, body};
  }

  function chatGuide(model, generic) {
    const caps = model.kanata;
    const {panel, details, body: content} = endpoint("/chat/completions", "Chat completions", "Send a conversation as application/json. A non-streaming response returns choices[0].message, finish_reason and usage when reported by the backend.");
    const rows = [
      ["model", `Required string. ${generic ? "Replace your-chat-model with an allowed alias." : `Use ${model.id}.`}`],
      ["messages", "Required, non-empty array of messages, for example {role: \"user\", content: \"Hello\"}. Text conversations support system, user and assistant roles."],
    ];
    if (generic || caps.streaming === true) rows.push(["stream / stream_options", "stream: true returns server-sent events ending with data: [DONE]. stream_options: {include_usage: true} requests a final usage chunk. Omit stream_options when not streaming."]);
    if (generic || caps.function_tools === true) rows.push(["tools / tool_choice", "Function definitions use type: function and function: {name, description, parameters}. tool_choice accepts auto, none, required or a named function. Tool results use role: tool and tool_call_id; your client executes tools."]);
    if (generic || caps.sampling_controls === true) rows.push(
      ["temperature / top_p / seed", "Optional numbers: temperature 0–2, top_p greater than 0 and at most 1, seed a signed integer."],
      ["max_tokens / max_completion_tokens", `Set only one, as a positive integer up to 1,048,576. ${caps.max_output_tokens ? `This route caps output at ${caps.max_output_tokens} tokens, also applied when omitted.` : "A configured route output cap may lower that limit."}`],
    );
    if (generic || caps.structured_output === true) rows.push(["response_format", "Supports {type: \"json_object\"} or {type: \"json_schema\", json_schema: {name, schema, strict}}. JSON schemas are limited to 64 KiB and depth 32."]);
    if (generic || caps.reasoning_control === true) rows.push(["reasoning_effort", `Allowed values: ${(caps.reasoning_efforts || ["none", "minimal", "low", "medium", "high", "xhigh", "max"]).join(", ")}. Backend support still depends on the selected model.`]);
    if (generic || caps.input_audio === true) rows.push(["messages[].content[].input_audio", "Audio parts use {type: \"input_audio\", input_audio: {data: \"BASE64_AUDIO\", format: \"wav\"}} (wav or mp3). Audio combined with streaming or tools needs separate route support; the text capability flags alone do not guarantee it."]);
    const requestFields = element("details", undefined, "request-fields");
    requestFields.append(element("summary", "Request fields"));
    fields(requestFields, rows);
    if (generic) details.append(element("p", "Optional fields require matching route capabilities. Connect your key to narrow this reference.", "muted"));
    const body = { model: model.id, messages: [{ role: "user", content: "Hello!" }] };
    examples(content, "Chat", [
      {label: "cURL", language: "shell", filename: "Terminal", setup: "Set KANATA_API_KEY in your environment before running this request.", source: curl("/chat/completions", body)},
      ...requestExamples(base, model.id),
    ]);
    details.append(requestFields);
    if (generic || caps.streaming === true) {
      const streaming = element("details", undefined, "request-fields");
      streaming.append(element("summary", "Streaming example"));
      const language = preferredLanguage;
      codeBlock(streaming, "Streaming", curl("/chat/completions", { ...body, stream: true, stream_options: { include_usage: true } }, true));
      preferredLanguage = language;
      panel.append(streaming);
    }
    return panel;
  }

  function transcriptionGuide(model) {
    const {panel, details, body: content} = endpoint("/audio/transcriptions", "Audio transcription", "Upload an audio file as multipart/form-data. The default response is JSON: {\"text\":\"…\"}. Transcription responses do not stream.");
    const requestFields = element("details", undefined, "request-fields");
    requestFields.append(element("summary", "Request fields"));
    fields(requestFields, [
      ["model", `Required alias: ${model.id}. Chat permission does not grant transcription permission.`],
      ["file", "Required audio file with a filename and audio media type. The configured upload limit applies."],
      ["response_format", "Optional: json (default) or text."],
      ["language / prompt", "Optional hints; support varies by backend. Unsupported hints are rejected. Omit them for a portable request."],
    ]);
    examples(content, "Transcription", [
      {label: "cURL", language: "shell", filename: "Terminal", setup: "Place sample.wav in your working directory and set KANATA_API_KEY.", source: `curl ${quote(base + "/audio/transcriptions")} \\\n${authHeader} \\\n  --form-string ${quote("model=" + model.id)} \\\n  -F 'file=@sample.wav;type=audio/wav'`},
      ...requestExamples(base, model.id, true),
    ]);
    details.append(requestFields);
    return panel;
  }

  function showGuide(model, generic = false) {
    const target = byId("model-guide");
    target.replaceChildren();
    if (!model) return;
    if (model.kanata.operations.includes("chat")) target.append(chatGuide(model, generic));
    if (model.kanata.operations.includes("transcription")) target.append(transcriptionGuide(model));
  }

  function genericGuide() {
    const generic = { id: "your-chat-model", kanata: { operations: ["chat"] } };
    showGuide(generic, true);
    byId("model-guide").append(transcriptionGuide({ id: "your-transcription-model" }));
  }

  function clearAccess() {
    models = [];
    select.replaceChildren();
    byId("model-picker").hidden = true;
    byId("models").replaceChildren(element("p", "Connect a key to see its models and supported operations.", "empty"));
    byId("model-count").textContent = "KEY REQUIRED";
    byId("connection").textContent = "Public reference";
    byId("connection").classList.remove("active");
    byId("scope-note").textContent = "Generic examples use placeholder aliases. Endpoint availability and optional features depend on your key and the configured routes.";
    genericGuide();
  }

  function disconnect() {
    requestNumber += 1;
    controller?.abort();
    key = "";
    input.value = "";
    input.disabled = !secure;
    form.hidden = false;
    byId("session").hidden = true;
    byId("connect").disabled = !secure;
    byId("refresh").disabled = false;
    status.textContent = secure ? "" : "Use HTTPS to connect a key. Localhost is allowed for development.";
    status.className = secure ? "" : "error";
    clearAccess();
  }

  function renderAccess() {
    byId("models").replaceChildren();
    for (const model of models) {
      const caps = model.kanata;
      const card = element("article", undefined, "model-card");
      card.append(element("h3", model.id));
      const badges = element("div", undefined, "badges");
      for (const operation of caps.operations) badges.append(element("span", operation, "badge operation"));
      for (const [flag, label] of [["streaming", "Text streaming"], ["function_tools", "Function tools"], ["structured_output", "Structured output"], ["sampling_controls", "Sampling"], ["reasoning_control", "Reasoning controls"], ["input_audio", "Audio input"]]) {
        if (caps[flag] === true) badges.append(element("span", label, "badge"));
      }
      card.append(badges);
      if (caps.context_tokens) card.append(element("p", `Context: ${caps.context_tokens.toLocaleString()} tokens`));
      if (caps.max_output_tokens) card.append(element("p", `Output cap: ${caps.max_output_tokens.toLocaleString()} tokens`));
      byId("models").append(card);
      const option = element("option", model.id);
      option.value = model.id;
      select.append(option);
    }
    byId("model-count").textContent = `${models.length} MODEL${models.length === 1 ? "" : "S"}`;
    byId("model-picker").hidden = models.length === 0;
    if (!models.length) byId("models").append(element("p", "This key has no available models on this listener.", "empty"));
    byId("scope-note").textContent = "Showing this key’s allowed operations and declared capabilities. This is a snapshot; refresh after changes. A listed model is not a backend health check.";
    showGuide(models[0]);
    byId("connection").textContent = "Key connected";
    byId("connection").classList.add("active");
  }

  async function loadAccess() {
    if (!secure || !key) return;
    controller?.abort();
    controller = new AbortController();
    const currentController = controller;
    const number = ++requestNumber;
    clearAccess();
    form.hidden = true;
    byId("session").hidden = false;
    byId("refresh").disabled = true;
    status.className = "";
    status.textContent = "Checking access…";
    const timeout = setTimeout(() => currentController.abort(), 15000);
    try {
      const response = await fetch("/v1/models", {
        headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
        mode: "same-origin", credentials: "omit", cache: "no-store", redirect: "error",
        signal: currentController.signal,
      });
      if (!response.ok) throw new GuideError([401, 403].includes(response.status) ? "Key rejected. Check its value, expiry and access to this listener." : `Unable to load access (HTTP ${response.status}).`);
      const data = await response.json();
      if (!Array.isArray(data.data) || !data.data.every((model) => typeof model.id === "string" && Array.isArray(model.kanata?.operations) && model.kanata.operations.every((op) => ["chat", "transcription"].includes(op)))) throw new GuideError("The gateway returned an unexpected model listing.");
      if (number !== requestNumber) return;
      models = data.data;
      renderAccess();
      status.textContent = "Access loaded. Disconnect to clear this key and its model details.";
    } catch (error) {
      if (number !== requestNumber) return;
      disconnect();
      status.className = "error";
      status.textContent = error.name === "AbortError" ? "The request timed out. Reconnect to try again." : error instanceof GuideError ? error.message : "Unable to read the gateway response. Check your connection and reconnect.";
    } finally {
      clearTimeout(timeout);
      if (number === requestNumber) byId("refresh").disabled = false;
    }
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!secure) return;
    key = input.value.trim().replace(/^Bearer\s+/i, "");
    input.value = "";
    if (!key) { status.textContent = "Enter a bearer key."; return; }
    void loadAccess();
  });
  byId("refresh").addEventListener("click", () => void loadAccess());
  byId("disconnect").addEventListener("click", () => { disconnect(); input.focus(); });
  select.addEventListener("change", () => showGuide(models.find((model) => model.id === select.value)));
  window.addEventListener("pagehide", disconnect);
  window.addEventListener("pageshow", disconnect);
  byId("host").textContent = location.host;
  byId("base-url").textContent = base;
  const modelsSource = `curl ${quote(base + "/models")} \\\n${authHeader}`;
  highlightCode(byId("models-example"), modelsSource, "shell");
  disconnect();
})();
