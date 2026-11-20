import { ImageResponse } from "next/og";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

export const alt = "komet — one control room for all your coding agents";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

const [logo, semibold, regular, mono] = await Promise.all([
  readFile(join(process.cwd(), "public/komet.png")),
  readFile(
    join(
      process.cwd(),
      "node_modules/geist/dist/fonts/geist-sans/Geist-SemiBold.ttf",
    ),
  ),
  readFile(
    join(
      process.cwd(),
      "node_modules/geist/dist/fonts/geist-sans/Geist-Regular.ttf",
    ),
  ),
  readFile(
    join(
      process.cwd(),
      "node_modules/geist/dist/fonts/geist-mono/GeistMono-Regular.ttf",
    ),
  ),
]);

const logoSrc = `data:image/png;base64,${logo.toString("base64")}`;

const transcript = [
  { who: "you", text: "refactor the auth middleware to use the new session store" },
  { who: "claude-code", text: "Reading crates/engine/src/auth.rs and 3 related files…" },
  { who: "claude-code", text: "Applying edits · 2 files changed, checkpoint 4f9a1c" },
];

const checkpoints = ["1a2f", "88de", "4f9a", "now"];

export default function Image() {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          backgroundColor: "#09090b",
          color: "#ececec",
          fontFamily: "Geist",
          padding: "64px",
          gap: "56px",
        }}
      >
        {/* Left: brand + copy */}
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            justifyContent: "space-between",
            flex: 1,
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: "16px" }}>
            <img src={logoSrc} width={44} height={44} alt="" />
            <span style={{ fontSize: 30, fontWeight: 600, letterSpacing: "-0.02em" }}>
              komet
            </span>
          </div>

          <div style={{ display: "flex", flexDirection: "column", gap: "24px" }}>
            <div
              style={{
                fontSize: 64,
                fontWeight: 600,
                letterSpacing: "-0.03em",
                lineHeight: 1.05,
                maxWidth: 620,
              }}
            >
              One control room for all your coding agents.
            </div>
            <div
              style={{
                fontSize: 24,
                color: "#9e9e9e",
                lineHeight: 1.4,
                maxWidth: 560,
              }}
            >
              Sessions, transcripts and checkpoints — in a single native window,
              entirely on your machine.
            </div>
          </div>

          <div
            style={{
              fontFamily: "Geist Mono",
              fontSize: 18,
              color: "#9e9e9e",
            }}
          >
            100% local · macOS · Linux · Windows
          </div>
        </div>

        {/* Right: session card */}
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            width: 460,
            borderRadius: 16,
            border: "1px solid rgba(255,255,255,0.08)",
            backgroundColor: "#13111a",
            overflow: "hidden",
          }}
        >
          {/* titlebar */}
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              padding: "14px 18px",
              borderBottom: "1px solid rgba(255,255,255,0.08)",
              backgroundColor: "rgba(19,17,26,0.6)",
            }}
          >
            <div style={{ display: "flex", gap: "8px" }}>
              {[0, 1, 2].map((i) => (
                <div
                  key={i}
                  style={{
                    width: 11,
                    height: 11,
                    borderRadius: 6,
                    backgroundColor: "#1a1820",
                    border: "1px solid rgba(255,255,255,0.08)",
                  }}
                />
              ))}
            </div>
            <span
              style={{
                fontFamily: "Geist Mono",
                fontSize: 14,
                color: "#9e9e9e",
              }}
            >
              auth-refactor · local
            </span>
            <span
              style={{
                fontFamily: "Geist Mono",
                fontSize: 14,
                color: "#9e9e9e",
                display: "flex",
                alignItems: "center",
                gap: 6,
              }}
            >
              <div
                style={{
                  width: 8,
                  height: 8,
                  borderRadius: 4,
                  backgroundColor: "#ececec",
                }}
              />
              live
            </span>
          </div>

          {/* transcript */}
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "16px",
              padding: "20px 18px",
              fontFamily: "Geist Mono",
              fontSize: 15,
              lineHeight: 1.5,
            }}
          >
            {transcript.map((line, i) => (
              <div key={i} style={{ display: "flex", gap: "10px" }}>
                <div
                  style={{
                    marginTop: 5,
                    width: 14,
                    height: 14,
                    borderRadius: 4,
                    backgroundColor: "#1a1820",
                    border: "1px solid rgba(255,255,255,0.08)",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                  }}
                >
                  <div
                    style={{
                      width: 6,
                      height: 6,
                      borderRadius: 3,
                      backgroundColor: line.who === "you" ? "#9e9e9e" : "#ececec",
                    }}
                  />
                </div>
                <div style={{ display: "flex" }}>
                  <span style={{ color: "#ececec", fontWeight: 500 }}>
                    {line.who}
                  </span>
                  <span style={{ color: "#9e9e9e" }}> · {line.text}</span>
                </div>
              </div>
            ))}
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                paddingLeft: 24,
                color: "#9e9e9e",
                fontSize: 14,
              }}
            >
              <div
                style={{
                  display: "flex",
                  gap: 5,
                }}
              >
                {[0, 1, 2].map((i) => (
                  <div
                    key={i}
                    style={{
                      width: 6,
                      height: 6,
                      borderRadius: 3,
                      backgroundColor: "#ececec",
                    }}
                  />
                ))}
              </div>
              working_
            </div>
          </div>

          {/* rewind scrubber */}
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: 10,
              padding: "14px 18px",
              borderTop: "1px solid rgba(255,255,255,0.08)",
              backgroundColor: "rgba(19,17,26,0.6)",
            }}
          >
            <div
              style={{
                display: "flex",
                justifyContent: "space-between",
                fontFamily: "Geist Mono",
                fontSize: 12,
                color: "#9e9e9e",
                textTransform: "uppercase",
              }}
            >
              <span>rewind</span>
              <span>4 checkpoints</span>
            </div>
            <div
              style={{
                position: "relative",
                display: "flex",
                height: 4,
                borderRadius: 2,
                backgroundColor: "#1a1820",
              }}
            >
              <div
                style={{
                  position: "absolute",
                  left: 0,
                  top: 0,
                  bottom: 0,
                  width: "78%",
                  borderRadius: 2,
                  backgroundColor: "rgba(236,236,236,0.6)",
                }}
              />
              {checkpoints.map((c, i) => (
                <div
                  key={c}
                  style={{
                    position: "absolute",
                    left: `${(i / (checkpoints.length - 1)) * 100}%`,
                    top: -4,
                    width: 10,
                    height: 10,
                    marginLeft: -5,
                    borderRadius: 5,
                    backgroundColor: i === 2 ? "#ececec" : "#1a1820",
                    border: "1px solid rgba(255,255,255,0.08)",
                  }}
                />
              ))}
            </div>
          </div>
        </div>
      </div>
    ),
    {
      ...size,
      fonts: [
        { name: "Geist", data: semibold, weight: 600 as const },
        { name: "Geist", data: regular, weight: 400 as const },
        { name: "Geist Mono", data: mono, weight: 400 as const },
      ],
    },
  );
}
