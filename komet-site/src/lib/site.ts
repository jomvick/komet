/** Canonical site origin — set NEXT_PUBLIC_SITE_URL in the deploy env. */
export const SITE_URL =
  process.env.NEXT_PUBLIC_SITE_URL ?? "http://localhost:3000";
