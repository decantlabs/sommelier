import { useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent, ReactNode } from "react";
import { ArrowsOut, Minus, Plus } from "@phosphor-icons/react";
import { Button } from "../components";

const dimensions = { width: 1520, height: 740, x: 760, y: 370 };
const minZoom = 0.35;
const maxZoom = 1.6;

function positions(count: number) {
  const outerCount = Math.min(count, 16);
  const innerCount = count - outerCount;
  return Array.from({ length: count }, (_, index) => {
    const inner = index >= outerCount;
    const slot = inner ? index - outerCount : index;
    const slots = inner ? innerCount : outerCount;
    const angle = (slot / slots) * Math.PI * 2 - Math.PI / 2 + (inner ? Math.PI / slots : 0);
    return {
      x: dimensions.x + Math.cos(angle) * (inner ? 310 : outerCount <= 8 ? 430 : 590),
      y: dimensions.y + Math.sin(angle) * (inner ? 148 : outerCount <= 8 ? 205 : 284),
    };
  });
}

export type ParticipantNode = { id: string; tone: "running" | "attention" | "quiet"; button: ReactNode };

export default function ParticipantTopology({ nodes, selected, hub }: {
  nodes: ParticipantNode[];
  selected?: string;
  hub: ReactNode;
}) {
  const viewport = useRef<HTMLDivElement>(null);
  const edges = useRef<HTMLCanvasElement>(null);
  const drag = useRef<{ startX: number; startY: number; left: number; top: number } | null>(null);
  const [isPanning, setPanning] = useState(false);
  const [viewSize, setViewSize] = useState({ width: dimensions.width, height: dimensions.height });
  const [manualZoom, setManualZoom] = useState<number | null>(null);
  const fit = Math.min(
    (viewSize.width - 32) / dimensions.width,
    (viewSize.height - 32) / dimensions.height,
    1.08,
  );
  const scale = manualZoom ?? Math.max(0.52, fit);
  const points = useMemo(() => positions(nodes.length), [nodes.length]);
  const hasRunning = nodes.some((n) => n.tone === "running");

  useEffect(() => {
    const element = viewport.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => {
      setViewSize({
        width: Math.max(280, entry.contentRect.width),
        height: Math.max(280, entry.contentRect.height),
      });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const element = viewport.current;
    if (!element) return;
    element.scrollTo((element.scrollWidth - element.clientWidth) / 2, (element.scrollHeight - element.clientHeight) / 2);
  }, [scale, viewSize.width, viewSize.height]);

  useEffect(() => {
    const canvas = edges.current;
    const context = canvas?.getContext("2d");
    if (!canvas || !context) return;

    const styles = getComputedStyle(document.documentElement);
    const base = styles.getPropertyValue("--color-line-edge").trim() || "#7e7694";
    const warning = styles.getPropertyValue("--color-warning-edge").trim() || "#eabc67";
    const good = styles.getPropertyValue("--color-good").trim() || "#79d8a5";
    const accent = styles.getPropertyValue("--color-accent").trim() || "#cdb4ff";
    const reducedMotion =
      typeof window !== "undefined" &&
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

    let rafId = 0;

    function draw(nowMs: number) {
      if (!context) return;
      context.setTransform(2, 0, 0, 2, 0, 0);
      context.clearRect(0, 0, dimensions.width, dimensions.height);

      const phase = ((nowMs % 3200) / 3200);

      points.forEach(({ x, y }, index) => {
        const node = nodes[index];
        const isSelected = selected === node.id;
        const warn = node.tone === "attention";
        const running = node.tone === "running";

        context.globalAlpha = isSelected ? 0.92 : running ? 0.65 : warn ? 0.55 : 0.28;
        context.strokeStyle = isSelected ? accent : running ? good : warn ? warning : base;
        context.setLineDash(warn && !isSelected ? [5, 6] : []);
        context.lineWidth = isSelected ? 1.8 : running ? 1.35 : 1;
        context.beginPath();
        context.moveTo(dimensions.x, dimensions.y);
        context.lineTo(x, y);
        context.stroke();

        // Calm, purposeful telemetry pulse along active running links only.
        if (running && !reducedMotion) {
          const t = (phase + index * 0.19) % 1;
          const px = dimensions.x + (x - dimensions.x) * t;
          const py = dimensions.y + (y - dimensions.y) * t;
          context.setLineDash([]);
          context.globalAlpha = Math.sin(t * Math.PI) * 0.9;
          context.fillStyle = good;
          context.beginPath();
          context.arc(px, py, 2.8, 0, Math.PI * 2);
          context.fill();
        }
      });

      context.globalAlpha = 1;
      context.setLineDash([]);
    }

    draw(performance.now());

    if (hasRunning && !reducedMotion) {
      const tick = (ts: number) => {
        draw(ts);
        rafId = window.requestAnimationFrame(tick);
      };
      rafId = window.requestAnimationFrame(tick);
    }

    return () => {
      if (rafId) window.cancelAnimationFrame(rafId);
    };
  }, [hasRunning, nodes, points, selected]);

  function startPan(event: PointerEvent<HTMLDivElement>) {
    const area = event.currentTarget;
    if (event.pointerType !== "mouse" || event.button !== 0 || (event.target as HTMLElement).closest("button")) return;
    if (area.scrollWidth <= area.clientWidth && area.scrollHeight <= area.clientHeight) return;
    drag.current = { startX: event.clientX, startY: event.clientY, left: area.scrollLeft, top: area.scrollTop };
    area.setPointerCapture(event.pointerId);
    setPanning(true);
  }

  function movePan(event: PointerEvent<HTMLDivElement>) {
    if (!drag.current) return;
    const { startX, startY, left, top } = drag.current;
    event.currentTarget.scrollTo(left + startX - event.clientX, top + startY - event.clientY);
  }

  function stopPan() {
    drag.current = null;
    setPanning(false);
  }

  return (
    <div className="topology">
      <div
        ref={viewport}
        className={`topology-viewport ${isPanning ? "is-panning" : ""}`}
        tabIndex={0}
        aria-label="Scrollable participant canvas"
        onPointerDown={startPan}
        onPointerMove={movePan}
        onPointerUp={stopPan}
        onPointerCancel={stopPan}
        onLostPointerCapture={stopPan}
      >
        <div
          className="topology-size"
          style={{
            width: dimensions.width * scale,
            height: dimensions.height * scale,
            marginTop: Math.max(0, (viewSize.height - dimensions.height * scale) / 2),
          }}
        >
          <div
            className="topology-world"
            style={{ width: dimensions.width, height: dimensions.height, transform: `scale(${scale})` }}
          >
            <canvas
              ref={edges}
              width={dimensions.width * 2}
              height={dimensions.height * 2}
              style={{ width: dimensions.width, height: dimensions.height }}
              aria-hidden="true"
            />
            <div className="topology-hub">{hub}</div>
            <ul className="network-nodes" aria-label="Participants in this hub">
              {nodes.map((node, index) => (
                <li key={node.id} style={{ left: points[index].x, top: points[index].y }}>
                  {node.button}
                </li>
              ))}
            </ul>
          </div>
        </div>
      </div>
      <div className="topology-controls" aria-label="Network canvas controls" role="group">
        <Button
          variant="icon"
          size="sm"
          aria-label="Zoom in on network"
          disabled={scale >= maxZoom}
          onClick={() => setManualZoom(Math.min(maxZoom, scale * 1.2))}
        >
          <Plus size={16} />
        </Button>
        <Button
          variant="icon"
          size="sm"
          aria-label="Zoom out on network"
          disabled={scale <= minZoom}
          onClick={() => setManualZoom(Math.max(minZoom, scale / 1.2))}
        >
          <Minus size={16} />
        </Button>
        <Button
          variant="icon"
          size="sm"
          aria-label="Fit network"
          onClick={() => setManualZoom(null)}
        >
          <ArrowsOut size={16} />
        </Button>
        <output aria-label="Network zoom">{Math.round(scale * 100)}%</output>
      </div>
    </div>
  );
}
