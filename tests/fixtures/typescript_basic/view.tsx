import { validateToken } from "./auth";

export function Badge() {
  return <span>{validateToken("abcdefghi") ? "ok" : "no"}</span>;
}
