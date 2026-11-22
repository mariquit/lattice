import { parseTokens } from "@core/parse";                // line 1

export function render(): number {                        // line 3
  return parseTokens("x y").length;
}
