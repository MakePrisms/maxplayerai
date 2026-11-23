/**
 * The team invite page (/join/<token> in a browser): the link the person hands to their agent, read from the
 * address. Kept apart from src/join.ts so the tests can run it without a page.
 */

// The token the registration service mints: 32 random bytes, base64url without padding (43 characters). Accept a
// little more than that, and nothing that is not base64url, so whatever lands in the line came from a real link.
const PATH = /^\/join\/([A-Za-z0-9_-]{20,128})\/?$/;

/** The full invite link for a /join/<token> path, or null for any other path. */
export function inviteLink(origin: string, pathname: string): string | null {
  const m = PATH.exec(pathname);
  return m ? `${origin}/join/${m[1]}` : null;
}

/** The line the person copies for their agent (the homepage's "Send your agent one line", for an invite). */
export function agentLine(link: string): string {
  return `Read ${link} and join this team for me`;
}
