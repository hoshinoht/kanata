function routeGroups(routes) {
  const groups = new Map();
  const efforts = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];
  routes.forEach((route, index) => {
    const alias = route.display_alias || route.model_alias;
    const provider = route.provider || 'unknown';
    const groupId = route.reasoning_effort ? JSON.stringify([provider, alias, route.operation]) : `route-${index}`;
    if (!groups.has(groupId)) groups.set(groupId, { provider, alias, operation: route.operation, exposure: route.exposure, levels: [], indices: [] });
    const group = groups.get(groupId);
    group.indices.push(index);
    if (route.reasoning_effort) {
      let level = group.levels.find((level) => level.effort === route.reasoning_effort);
      if (!level) { level = { effort: route.reasoning_effort, indices: [] }; group.levels.push(level); }
      level.indices.push(index);
    }
  });
  for (const group of groups.values()) group.levels.sort((a, b) => efforts.indexOf(a.effort) - efforts.indexOf(b.effort));
  return [...groups.values()];
}

function providerGroups(routes) {
  const labels = {chatgpt: 'ChatGPT', codex: 'Codex', ollama: 'Ollama', vllm: 'vLLM', openrouter: 'OpenRouter', apple_fm: 'Apple Foundation Models', speech: 'Speech', unknown: 'Other providers'};
  const providers = new Map();
  for (const group of routeGroups(routes)) {
    if (!providers.has(group.provider)) providers.set(group.provider, {id: group.provider, label: labels[group.provider] || group.provider, models: []});
    providers.get(group.provider).models.push(group);
  }
  return [...providers.values()].sort((a, b) => a.label.localeCompare(b.label));
}
