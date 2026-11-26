import { useEffect, useRef, useState } from "react";
import type { PointerEvent, ReactNode } from "react";
import { ArrowsOut, Minus, Plus } from "@phosphor-icons/react";
import { Button } from "../components";

const dimensions = { width: 1440, height: 720, x: 720, y: 360 };
const minZoom = 0.25;
const maxZoom = 1.8;

function positions(count: number) {
  const outerCount = Math.min(count, 16);
  const innerCount = count - outerCount;
  return Array.from({ length: count }, (_, index) => {
    const inner = index >= outerCount;
    const slot = inner ? index - outerCount : index;
    const slots = inner ? innerCount : outerCount;
    const angle = (slot / slots) * Math.PI * 2 - Math.PI / 2 + (inner ? Math.PI / slots : 0);
    return {
      x: dimensions.x + Math.cos(angle) * (inner ? 292 : outerCount < 8 ? 390 : 570),
      y: dimensions.y + Math.sin(angle) * (inner ? 134 : outerCount < 8 ? 167 : 278),
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
  const fit = Math.min(viewSize.width / dimensions.width, viewSize.height / dimensions.height, 1.2);
  const scale = manualZoom ?? Math.max(0.6, fit);
  const points = positions(nodes.length);

  useEffect(() => {
    const element = viewport.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => setViewSize({ width: entry.contentRect.width, height: entry.contentRect.height }));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const element = viewport.current;
    if (!element) return;
    element.scrollTo((element.scrollWidth - element.clientWidth) / 2, (element.scrollHeight - element.clientHeight) / 2);
  }, [scale, viewSize.width, viewSize.height]);

  useEffect(() => {
    const context = edges.current?.getContext("2d");
    if (!context) return;
    const styles = getComputedStyle(document.documentElement);
    const base = styles.getPropertyValue("--color-line-edge").trim();
    const warning = styles.getPropertyValue("--color-warning-edge").trim();
    const accent = styles.getPropertyValue("--color-accent").trim();
    context.setTransform(2, 0, 0, 2, 0, 0);
    context.clearRect(0, 0, dimensions.width, dimensions.height);
    points.forEach(({ x, y }, index) => {
      const node = nodes[index];
      const isSelected = selected === node.id;
      const warn = node.tone === "attention";
      context.globalAlpha = isSelected ? 0.9 : warn ? 0.5 : 0.33;
      context.strokeStyle = isSelected ? accent : warn ? warning : base;
      context.setLineDash(warn && !isSelected ? [4, 6] : []);
      context.lineWidth = isSelected ? 1.8 : 1;
      context.beginPath();
      context.moveTo(dimensions.x, dimensions.y);
      context.lineTo(x, y);
      context.stroke();
    });
  }, [nodes, points, selected]);

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
      <div ref={viewport} className={`topology-viewport ${isPanning ? "is-panning" : ""}`}
        tabIndex={0} aria-label="Scrollable participant canvas" onPointerDown={startPan}
        onPointerMove={movePan} onPointerUp={stopPan} onPointerCancel={stopPan} onLostPointerCapture={stopPan}>
        <div className="topology-size" style={{
          width: dimensions.width * scale,
          height: dimensions.height * scale,
          marginTop: Math.max(0, (viewSize.height - dimensions.height * scale) / 2),
        }}>
          <div className="topology-world" style={{ width: dimensions.width, height: dimensions.height, transform: `scale(${scale})` }}>
            <canvas ref={edges} width={dimensions.width * 2} height={dimensions.height * 2}
              style={{ width: dimensions.width, height: dimensions.height }} aria-hidden="true" />
            <div className="topology-hub">{hub}</div>
            <ul className="network-nodes" aria-label="Participants in this hub">
              {nodes.map((node, index) => (
                <li key={node.id} style={{ left: points[index].x, top: points[index].y }}>{node.button}</li>
              ))}
            </ul>
          </div>
        </div>
      </div>
      <div className="topology-controls" aria-label="Network canvas controls" role="group">
        <Button variant="icon" size="sm" aria-label="Zoom in on network" disabled={scale >= maxZoom}
          onClick={() => setManualZoom(Math.min(maxZoom, scale * 1.25))}><Plus size={17} /></Button>
        <Button variant="icon" size="sm" aria-label="Zoom out on network" disabled={scale <= minZoom}
          onClick={() => setManualZoom(Math.max(minZoom, scale / 1.25))}><Minus size={17} /></Button>
        <Button variant="icon" size="sm" aria-label="Fit network" onClick={() => setManualZoom(Math.min(1.2, Math.max(minZoom, fit)))}>
          <ArrowsOut size={17} />
        </Button>
        <output aria-label="Network zoom">{Math.round(scale * 100)}%</output>
      </div>
    </div>
  );
}
