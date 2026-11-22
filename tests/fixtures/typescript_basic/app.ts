import { validateToken } from "./auth";
import { saveSession } from "./db";

export function main(): void {
  if (validateToken("abcdefghi")) {
    saveSession(1);
  }
}
