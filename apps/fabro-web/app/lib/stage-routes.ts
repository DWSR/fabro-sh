/** Build a stage link from the original occurrence ID, never an encoded ID. */
export function stagePath(runId: string, stageId: string): string {
  return `/runs/${encodeURIComponent(runId)}/stages/${encodeURIComponent(stageId)}`;
}

/**
 * Read the original URL segment once. React Router's params replace literal
 * `%2F` with `/` after decoding, so they cannot distinguish those stage IDs.
 * Pass useLocation().pathname, which excludes the query and fragment.
 */
export function stageIdFromPathname(pathname: string): string | undefined {
  const segment = pathname.match(/\/stages\/([^/]+)\/*$/i)?.[1];
  if (segment === undefined) return undefined;
  try {
    return decodeURIComponent(segment);
  } catch {
    return undefined;
  }
}
