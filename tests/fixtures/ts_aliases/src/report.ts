import { parseTokens } from "./core/parse.js";            // line 1

export function report(): string {                        // line 3
  return parseTokens("p q").join(",");
}
