/** A command rejection is an `Error`, a thrown string, or anything else; render
 *  whichever carries a `message` before falling back to `String`. */
export function errMessage(e: unknown): string {
  return e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : String(e);
}
