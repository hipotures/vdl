const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function byName(left, right) {
  return collator.compare(left.account, right.account)
    || collator.compare(left.service, right.service)
    || collator.compare(left.id, right.id);
}

export function sortSources(sources, mode) {
  const sorted = [...sources];
  if (mode === "name") return sorted.sort(byName);
  return sorted.sort((left, right) => {
    const leftRunning = ["downloading", "finishing"].includes(left.state);
    const rightRunning = ["downloading", "finishing"].includes(right.state);
    if (leftRunning !== rightRunning) return leftRunning ? -1 : 1;
    const leftCheck = left.last_check ?? -Infinity;
    const rightCheck = right.last_check ?? -Infinity;
    return rightCheck - leftCheck || byName(left, right);
  });
}
