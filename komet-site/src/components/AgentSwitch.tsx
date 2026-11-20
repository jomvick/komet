"use client";

import { motion } from "framer-motion";
import { ClaudeCode, Codex } from "@lobehub/icons";
import type { ReactNode } from "react";
import { allAgents } from "@/lib/agents";

const easing = [0.16, 1, 0.3, 1] as const;

function AgentMark({ Icon, size = 14 }: { Icon: (typeof allAgents)[number]["Icon"]; size?: number }) {
  if (!Icon) {
    return (
      <span
        aria-hidden="true"
        className="flex items-center justify-center font-mono text-[10px] font-medium"
        style={{ width: size, height: size }}
      >
        M
      </span>
    );
  }
  return <Icon size={size} />;
}

function Message({
  name,
  Icon,
  children,
}: {
  name: string;
  Icon: (typeof allAgents)[number]["Icon"];
  children: ReactNode;
}) {
  return (
    <div className="flex gap-3">
      <div className="mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-full border border-border bg-background text-muted-foreground">
        <AgentMark Icon={Icon} size={13} />
      </div>
      <div className="min-w-0">
        <p className="text-[11px] font-medium text-foreground/80">{name}</p>
        <p className="mt-0.5 text-[13px] leading-relaxed text-muted-foreground">
          {children}
        </p>
      </div>
    </div>
  );
}

export default function AgentSwitch() {
  return (
    <section className="px-5 py-20 md:px-10 md:py-28">
      <div className="grid items-center gap-14 lg:grid-cols-2">
        {/* Visual: the switch as it lands in the transcript */}
        <motion.div
          initial={{ opacity: 0, y: 20 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.7, ease: easing }}
          className="rounded-2xl border border-border bg-charcoal/30 p-5 md:p-6"
        >
          <div className="flex items-center justify-between border-b border-border pb-3">
            <p className="text-[12.5px] font-medium text-foreground/80">
              Refactor auth middleware
            </p>
            <p className="font-mono text-[10px] text-muted-foreground/60">
              komet / web-app
            </p>
          </div>

          <div className="mt-4 space-y-4">
            <div className="flex justify-end">
              <p className="max-w-[85%] rounded-xl rounded-br-sm border border-border bg-background/70 px-3.5 py-2 text-[13px] leading-relaxed text-foreground/90">
                Refactor the auth middleware and run the tests.
              </p>
            </div>

            <Message name="Claude Code" Icon={ClaudeCode}>
              Split it into session and token modules.{" "}
              <span className="text-muted-foreground/60">
                3 files changed · tests pass
              </span>
            </Message>

            <motion.div
              initial={{ opacity: 0, scaleX: 0.6 }}
              whileInView={{ opacity: 1, scaleX: 1 }}
              viewport={{ once: true, margin: "-60px" }}
              transition={{ duration: 0.5, delay: 0.25, ease: easing }}
              className="flex items-center gap-2 py-1"
            >
              <span className="h-px flex-1 bg-border" />
              <span className="flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 font-mono text-[10px] text-muted-foreground/70">
                <ClaudeCode size={12} />
                switched to
                <Codex size={12} className="text-foreground/80" />
                <span className="text-foreground/80">Codex</span>
              </span>
              <span className="h-px flex-1 bg-border" />
            </motion.div>

            <Message name="Codex" Icon={Codex}>
              Added refresh-token rotation and its test.{" "}
              <span className="text-muted-foreground/60">1 file changed</span>
            </Message>
          </div>
        </motion.div>

        {/* Copy */}
        <motion.div
          initial={{ opacity: 0, y: 14 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.6, ease: easing }}
        >
          <div className="font-mono text-[11px] tracking-[0.14em] text-muted-foreground/80 uppercase">
            Thirteen agents
          </div>
          <h2 className="mt-3 text-[30px] leading-tight font-medium tracking-tight text-foreground md:text-[36px]">
            Change the agent, keep the conversation
          </h2>
          <p className="mt-4 max-w-md text-[15px] leading-relaxed text-muted-foreground">
            Thirteen agent CLIs in one window. Switch mid-chat and Komet writes
            a divider into the transcript, then runs the next turn on the new
            agent — same chat, same project, same memory.
          </p>

          <div className="mt-7 flex flex-wrap items-center gap-x-4 gap-y-3">
            {allAgents.map(({ name, Icon }) => (
              <span
                key={name}
                title={name}
                className="text-muted-foreground/60 transition-colors hover:text-foreground"
              >
                <AgentMark Icon={Icon} size={18} />
                <span className="sr-only">{name}</span>
              </span>
            ))}
          </div>
          <p className="mt-4 font-mono text-[10.5px] text-muted-foreground/60">
            13 agents · one timeline · switchable per chat
          </p>
        </motion.div>
      </div>
    </section>
  );
}
