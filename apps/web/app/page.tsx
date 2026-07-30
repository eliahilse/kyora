export default function Home() {
  return (
    <main
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        gap: 16,
        textAlign: "center",
      }}
    >
      <h1
        style={{
          fontSize: "clamp(2rem, 5vw, 4rem)",
          letterSpacing: "-0.02em",
          lineHeight: 1.1,
        }}
      >
        Kyora
      </h1>
      <p
        style={{
          fontSize: "clamp(0.875rem, 1.5vw, 1.25rem)",
          opacity: 0.6,
          maxWidth: 480,
          lineHeight: 1.5,
        }}
      >
        Superhuman debugging for agents.
      </p>
      <p
        style={{
          position: "fixed",
          bottom: 40,
          fontSize: "clamp(0.625rem, 1vw, 0.8rem)",
          opacity: 0.35,
          maxWidth: 400,
          lineHeight: 1.6,
        }}
      >
        Kyora is closing the gap in coding context, enabling agents to operate
        on a new level of awareness. SDK soon — by{" "}
        <a
          href="https://x.com/eliahilse"
          target="_blank"
          rel="noopener noreferrer"
          style={{ color: "inherit", textDecoration: "underline" }}
        >
          Elia Hilse
        </a>
        .
      </p>
    </main>
  );
}
