/** Substring filter for the model picker. Case-insensitive, stable order. */
export function filterModels(models: string[], query: string): string[] {
  const q = query.trim().toLowerCase();
  if (!q) return models;
  return models.filter((m) => m.toLowerCase().includes(q));
}
