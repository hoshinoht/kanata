function routeGroups(routes) {
  const groups = new Map();
  const efforts = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];
  routes.forEach((route, index) => {
    const alias = route.display_alias || route.model_alias;
    const groupId = route.reasoning_effort ? JSON.stringify([alias, route.operation]) : `route-${index}`;
    if (!groups.has(groupId)) groups.set(groupId, { alias, operation: route.operation, exposure: route.exposure, levels: [], indices: [] });
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
