"use client";

import { useEffect, useState } from "react";

const REPO = "jomvick/komet";
const API_URL = `https://api.github.com/repos/${REPO}/releases/latest`;

export type ReleaseInfo = {
  version: string;
  assets: string[];
};

export function assetUrl(version: string, name: string) {
  return `https://github.com/${REPO}/releases/download/v${version}/${name}`;
}

/**
 * Fetches the latest GitHub release so the download menu always reflects the
 * real assets — no hardcoded version, no dead links. Falls back to the
 * releases page when the API is unreachable.
 */
export function useLatestRelease(): ReleaseInfo | null {
  const [info, setInfo] = useState<ReleaseInfo | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch(API_URL)
      .then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
      .then((data) => {
        if (cancelled || typeof data?.tag_name !== "string") return;
        setInfo({
          version: data.tag_name.replace(/^v/, ""),
          assets: Array.isArray(data.assets)
            ? data.assets.map((a: { name?: string }) => a.name).filter(Boolean)
            : [],
        });
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  return info;
}
