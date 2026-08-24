"use client";

import { useState } from "react";
import { faq } from "@/lib/data";

export default function Faq() {
  const [open, setOpen] = useState<number | null>(null);

  return (
    <section className="border-t px-5 py-16 md:px-10">
      <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
        Questions
      </div>

      <div className="mt-6 flex w-full flex-col max-w-2xl">
        {faq.map((item, i) => {
          const isOpen = open === i;
          return (
            <div key={item.q} className="not-last:border-b border-border">
              <h3 className="flex">
                <button
                  type="button"
                  onClick={() => setOpen(isOpen ? null : i)}
                  aria-expanded={isOpen}
                  className="relative flex flex-1 items-start justify-between py-2.5 text-left font-medium transition-all outline-none hover:underline text-[15px]"
                >
                  {item.q}
                  <svg
                    xmlns="http://www.w3.org/2000/svg"
                    width="16"
                    height="16"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    className={`ml-4 shrink-0 text-muted-foreground transition-transform duration-200 ${isOpen ? "rotate-180" : ""}`}
                    aria-hidden="true"
                  >
                    <path d="m6 9 6 6 6-6" />
                  </svg>
                </button>
              </h3>
              {isOpen && (
                <p className="pb-4 text-sm leading-relaxed text-muted-foreground">
                  {item.a}
                </p>
              )}
            </div>
          );
        })}
      </div>
    </section>
  );
}

