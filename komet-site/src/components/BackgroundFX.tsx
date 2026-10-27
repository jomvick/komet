export default function BackgroundFX() {
  return (
    <div aria-hidden className="pointer-events-none fixed inset-0 -z-10 overflow-hidden">
      {/* subtle vignette + stars */}
      <div className="absolute inset-0 bg-[radial-gradient(ellipse_at_top,_rgba(255,255,255,0.06),transparent_60%),radial-gradient(ellipse_at_bottom,_rgba(255,255,255,0.03),transparent_70%)]" />
      {/* twinkling stars via box-shadow */}
      <div className="stars" />
      <div className="stars2" />
      {/* comet every 60s */}
      <div className="comet" />
      <style>{`
        .stars, .stars2 {
          position: absolute; inset: 0;
          background-image:
            radial-gradient(1px 1px at 10% 15%, rgba(255,255,255,0.9) 50%, transparent 55%),
            radial-gradient(1px 1px at 25% 8%, rgba(255,255,255,0.7) 50%, transparent 55%),
            radial-gradient(1px 1px at 40% 20%, rgba(255,255,255,0.6) 50%, transparent 55%),
            radial-gradient(1px 1px at 55% 12%, rgba(255,255,255,0.8) 50%, transparent 55%),
            radial-gradient(1px 1px at 70% 18%, rgba(255,255,255,0.5) 50%, transparent 55%),
            radial-gradient(1px 1px at 85% 10%, rgba(255,255,255,0.7) 50%, transparent 55%),
            radial-gradient(1px 1px at 15% 45%, rgba(255,255,255,0.5) 50%, transparent 55%),
            radial-gradient(1px 1px at 35% 55%, rgba(255,255,255,0.6) 50%, transparent 55%),
            radial-gradient(1px 1px at 65% 50%, rgba(255,255,255,0.45) 50%, transparent 55%),
            radial-gradient(1.2px 1.2px at 80% 60%, rgba(255,255,255,0.7) 50%, transparent 55%),
            radial-gradient(1px 1px at 20% 80%, rgba(255,255,255,0.4) 50%, transparent 55%),
            radial-gradient(1px 1px at 50% 85%, rgba(255,255,255,0.5) 50%, transparent 55%);
          animation: twinkle 4s ease-in-out infinite alternate;
        }
        .stars2 { opacity: 0.6; animation-duration: 6s; animation-delay: 1s; filter: blur(0.3px); }
        @keyframes twinkle { 0% { opacity: 0.5 } 100% { opacity: 1 } }
        .comet {
          position: absolute;
          top: -10%; left: -20%;
          width: 140px; height: 2px;
          background: linear-gradient(90deg, transparent, rgba(255,255,255,0.95) 55%, white);
          box-shadow: 0 0 8px rgba(255,255,255,0.9), 0 0 18px rgba(255,255,255,0.35);
          border-radius: 999px;
          transform: rotate(28deg);
          opacity: 0;
          animation: comet-fly 1.8s ease-in 5s infinite;
          animation-iteration-count: infinite;
          animation-delay: 3s;
          /* 60s interval: duration 1.8s + delay 58.2s => use long animation with keyframes */
          animation: comet-cycle 60s linear infinite;
        }
        @keyframes comet-cycle {
          0% { transform: translate3d(-20vw,-15vh,0) rotate(28deg); opacity: 0; }
          1% { opacity: 0; }
          2% { opacity: 1; }
          4.5% { transform: translate3d(120vw,85vh,0) rotate(28deg); opacity: 1; }
          5% { opacity: 0; }
          100% { transform: translate3d(120vw,85vh,0) rotate(28deg); opacity: 0; }
        }
        @media (prefers-reduced-motion: reduce) {
          .stars, .stars2, .comet { animation: none !important; }
        }
      `}</style>
    </div>
  );
}
