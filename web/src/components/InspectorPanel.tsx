import type { CSSProperties, ReactNode } from "react";
import { X } from "@phosphor-icons/react";
import { Button } from "./Button";

export type DefinitionItem = {
  label: string;
  value: ReactNode;
};

export function DefinitionList({
  items,
  className = "",
}: {
  items: DefinitionItem[];
  className?: string;
}) {
  return (
    <dl className={`ui-dl ${className}`.trim()}>
      {items.map((item) => (
        <div key={item.label} className="ui-dl-row">
          <dt>{item.label}</dt>
          <dd>{item.value}</dd>
        </div>
      ))}
    </dl>
  );
}

export function CodeChip({
  small = false,
  className = "",
  children,
}: {
  small?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <code className={`ui-code-chip ${small ? "ui-code-chip-sm" : ""} ${className}`.trim()}>
      {children}
    </code>
  );
}

export function InspectorSection({
  title,
  icon,
  children,
}: {
  title: string;
  icon?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="ui-inspector-section" aria-label={title}>
      <h3>
        {icon}
        {title}
      </h3>
      {children}
    </section>
  );
}

export function InspectorPanel({
  mode = "docked",
  ariaLabel = "Inspector",
  eyebrow,
  title,
  style,
  hasPointer = false,
  fromRight = false,
  onClose,
  closeLabel = "Close details",
  className = "",
  children,
}: {
  mode?: "floating" | "docked";
  ariaLabel?: string;
  eyebrow?: ReactNode;
  title?: ReactNode;
  style?: CSSProperties;
  hasPointer?: boolean;
  fromRight?: boolean;
  onClose?: () => void;
  closeLabel?: string;
  className?: string;
  children: ReactNode;
}) {
  if (mode === "floating") {
    return (
      <aside
        aria-label={ariaLabel}
        style={style}
        className={[
          "ui-inspector-floating",
          hasPointer ? "has-pointer" : "",
          fromRight ? "from-right" : "",
          className,
        ]
          .filter(Boolean)
          .join(" ")}
      >
        <div className="ui-inspector-top">
          {eyebrow}
          {onClose && (
            <Button variant="icon" aria-label={closeLabel} onClick={onClose}>
              <X size={17} />
            </Button>
          )}
        </div>
        {title && <h2>{title}</h2>}
        {children}
      </aside>
    );
  }

  return (
    <aside aria-label={ariaLabel} className={`ui-inspector-docked ${className}`.trim()}>
      {(eyebrow || onClose) && (
        <div className="ui-inspector-docked-head">
          <div className="ui-inspector-docked-eyebrow">{eyebrow}</div>
          {onClose && (
            <Button variant="icon" size="sm" aria-label={closeLabel} onClick={onClose}>
              <X size={16} />
            </Button>
          )}
        </div>
      )}
      <div className="ui-inspector-docked-body">
        {title && <h2>{title}</h2>}
        {children}
      </div>
    </aside>
  );
}
