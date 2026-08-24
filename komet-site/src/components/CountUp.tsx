"use client";

import { useEffect, useRef, useState } from "react";

function parseValue(raw: string) {
  const match = raw.match(/-?\d+(?:\.\d+)?/);
  if (!match || match.index === undefined) {
    return { prefix: raw, number: null as number | null, suffix: "", decimals: 0 };
  }
  const number = parseFloat(match[0]);
  const decimals = match[0].includes(".") ? match[0].split(".")[1].length : 0;
  return {
    prefix: raw.slice(0, match.index),
    number,
    suffix: raw.slice(match.index + match[0].length),
    decimals,
  };
}

const easeOutCubic = (t: number) => 1 - Math.pow(1 - t, 3);

export default function CountUp({
  value,
  duration = 1.1,
}: {
  value: string;
  duration?: number;
}) {
  const { prefix, number, suffix, decimals } = parseValue(value);
  const ref = useRef<HTMLSpanElement>(null);
  const [display, setDisplay] = useState(number === null ? "" : "0");
  const [started, setStarted] = useState(false);

  useEffect(() => {
    if (number === null) return;
    const el = ref.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setStarted(true);
          observer.disconnect();
        }
      },
      { threshold: 0.4 },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [number]);

  useEffect(() => {
    if (!started || number === null) return;
    let raf: number;
    const start = performance.now();

    const tick = (now: number) => {
      const t = Math.min((now - start) / (duration * 1000), 1);
      const current = number * easeOutCubic(t);
      setDisplay(decimals ? current.toFixed(decimals) : String(Math.round(current)));
      if (t < 1) raf = requestAnimationFrame(tick);
    };

    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [started, number, duration, decimals]);

  if (number === null) {
    return <span ref={ref}>{value}</span>;
  }

  return (
    <span ref={ref}>
      {prefix}
      {display}
      {suffix}
    </span>
  );
}
