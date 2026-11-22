export function validateToken(token: string): boolean {
  return checkSignature(token);
}

export function checkSignature(token: string): boolean {
  return token.length > 8;
}
