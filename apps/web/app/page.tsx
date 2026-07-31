import { AsciiField } from "./ascii-field";

const PRODUCTS = [
  {
    name: "state",
    line: "What actually happened at runtime, not what the code implies.",
  },
  {
    name: "review",
    line: "Every model family reviewing your PR together, consensus-ranked.",
  },
  {
    name: "council",
    line: "Your agent summoning other lineages mid-work, before it commits.",
  },
];

export default function Home() {
  return (
    <>
      <AsciiField />
      <main className="shell">
        <h1 className="headline">Kyora</h1>

        <p className="deck">Unlocking the full potential of coding agents.</p>

        <ul className="products">
          {PRODUCTS.map((product) => (
            <li key={product.name}>
              <span className="product-name">{product.name}</span>
              <span className="product-line">{product.line}</span>
            </li>
          ))}
        </ul>

        <footer className="footer">
          <a href="https://github.com/eliahilse/kyora" target="_blank" rel="noopener noreferrer">
            github
          </a>
          <span aria-hidden="true">·</span>
          <a href="https://x.com/eliahilse" target="_blank" rel="noopener noreferrer">
            elia hilse
          </a>
        </footer>
      </main>
    </>
  );
}
