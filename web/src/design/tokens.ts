export type CanvasTokens = {
  edge: string;
  conflict: string;
  portFill: string;
};

const DEFAULT_CANVAS_TOKENS: CanvasTokens = {
  edge: "#928aa5",
  conflict: "#eabc67",
  portFill: "#17141e",
};

/** Reads resolved CSS custom properties for imperative `<canvas>` graph rendering. */
export function readCanvasTokens(element: HTMLElement | null): CanvasTokens {
  if (!element || typeof window === "undefined") {
    return DEFAULT_CANVAS_TOKENS;
  }
  const styles = window.getComputedStyle(element);
  const edge = styles.getPropertyValue("--color-line-edge").trim();
  const conflict = styles.getPropertyValue("--color-warning-edge").trim();
  const portFill = styles.getPropertyValue("--color-surface-port").trim();
  return {
    edge: edge || DEFAULT_CANVAS_TOKENS.edge,
    conflict: conflict || DEFAULT_CANVAS_TOKENS.conflict,
    portFill: portFill || DEFAULT_CANVAS_TOKENS.portFill,
  };
}
