// Lightweight lexical highlighting for the guide's generated examples.
function highlightCode(code, source, language) {
  const words = {
    javascript: "await const let if throw new import from async function",
    python: "import from with as if raise print",
    go: "package import func if return defer var nil",
    rust: "use fn let mut return Ok Err Some None",
    shell: "curl export",
  };
  const keywords = new Set((words[language] || "").split(" "));
  const pattern = /("(?:\\[\s\S]|[^"\\])*"|'(?:\\[\s\S]|[^'\\])*'|`(?:\\[\s\S]|[^`\\])*`)|(\/\/[^\n]*|#[^\n]*)|\b(\d+(?:\.\d+)?)\b|\b([A-Za-z_][A-Za-z_0-9]*)\b/g;
  let offset = 0;
  for (const match of source.matchAll(pattern)) {
    code.append(document.createTextNode(source.slice(offset, match.index)));
    const type = match[1] ? "string" : match[2] ? "comment" : match[3] ? "number"
      : keywords.has(match[4]) ? "keyword" : /^(true|false|null|True|False|None)$/.test(match[4]) ? "literal" : "";
    const token = document.createElement("span");
    if (type) token.className = `token-${type}`;
    token.textContent = match[0];
    code.append(token);
    offset = match.index + match[0].length;
  }
  code.append(document.createTextNode(source.slice(offset)));
}
