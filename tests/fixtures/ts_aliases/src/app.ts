import { parseTokens, Parser } from "@core";              // line 1

const result = parseTokens("x");                          // line 3
                                                          // line 4
export function runApp(): string[] {                      // line 5
  return Parser.parse(result[0]);                         // line 6
}
