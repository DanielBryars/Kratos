/// Reading and writing invitation links, kept out of the component so the security-relevant part
/// can be tested directly.
///
/// ADR-017 puts the secret in the URL *fragment* and nowhere else. A fragment is not sent to the
/// server, so it stays out of load-balancer logs, access logs and `Referer` headers, where a
/// query string would outlive the invitation by the log retention period. Everything here exists
/// to keep that true: the credential is read from the fragment only, the address bar is cleaned
/// the moment it has been read, and it is never written to storage of any kind.

/// The prefix the control plane issues. Checking it here means a link carrying some other
/// credential -- a worker enrolment, say -- is refused in the browser rather than sent onward.
const INVITATION_PREFIX = "kin_";

/// The shape `credentials::issue` produces: prefix, a UUID, and a 43-character secret.
const INVITATION_PATTERN =
  /^kin_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}_[A-Za-z0-9_-]{43}$/;

export type InvitationLocation = {
  hash: string;
  search?: string;
};

/// Pull an invitation credential out of a location, or return null.
///
/// Only the fragment is consulted. A credential in the query string is ignored rather than
/// honoured, because honouring it would reward the one way of sending an invitation that leaks
/// it, and because a link shaped that way was not produced by this application.
export function readInvitationCredential(location: InvitationLocation): string | null {
  const fragment = location.hash.startsWith("#") ? location.hash.slice(1) : location.hash;
  if (!fragment) return null;

  // Accepts both `#kin_...` and `#invitation=kin_...`, since a link may be retyped or reassembled
  // by a mail client that treats the fragment as a parameter list.
  const candidate = fragment.includes("=")
    ? new URLSearchParams(fragment).get("invitation")
    : fragment;
  if (!candidate) return null;

  const trimmed = candidate.trim();
  if (!trimmed.startsWith(INVITATION_PREFIX)) return null;
  if (!INVITATION_PATTERN.test(trimmed)) return null;
  return trimmed;
}

/// The URL to hand someone, with the credential in the fragment.
export function buildInvitationLink(origin: string, credential: string): string {
  const base = origin.endsWith("/") ? origin.slice(0, -1) : origin;
  return `${base}/#${credential}`;
}

/// The URL to put in the address bar once the credential has been read into memory.
///
/// Returned rather than applied so the caller decides when to call `history.replaceState`, and so
/// this can be tested without a browser. `replaceState` rather than `pushState`: the address with
/// the secret in it must not become an entry the back button can return to.
export function scrubbedUrl(href: string): string {
  const url = new URL(href);
  url.hash = "";
  // `URL` leaves a trailing "#" behind when the hash is cleared, which would be both untidy and
  // a hint that something was removed.
  return url.toString().replace(/#$/, "");
}

/// How long an invitation has left, in words, for the pending list.
export function describeExpiry(expiresAt: string, now: Date = new Date()): string {
  const expiry = new Date(expiresAt);
  const milliseconds = expiry.getTime() - now.getTime();
  if (Number.isNaN(milliseconds)) return "expiry unknown";
  if (milliseconds <= 0) return "expired";
  const minutes = Math.round(milliseconds / 60_000);
  if (minutes < 60) return `expires in ${minutes} ${minutes === 1 ? "minute" : "minutes"}`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `expires in ${hours} ${hours === 1 ? "hour" : "hours"}`;
  const days = Math.round(hours / 24);
  return `expires in ${days} days`;
}
