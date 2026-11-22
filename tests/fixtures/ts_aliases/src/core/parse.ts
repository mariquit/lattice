export function parseTokens(input: string): string[] {   // line 1
  return input.split(" ");
}

export class Parser {                                     // line 5
  parse(input: string): string[] {                        // line 6
    return parseTokens(input);
  }
}
