import type { ReactNode } from "react";
import { FunnelSimple } from "@phosphor-icons/react";

export type SegmentOption<T extends string> = {
  value: T;
  label: string;
};

export function SegmentedControl<T extends string>({
  options,
  value,
  onChange,
  ariaLabel,
  size = "md",
  className = "",
}: {
  options: readonly SegmentOption<T>[];
  value: T;
  onChange: (value: T) => void;
  ariaLabel: string;
  size?: "sm" | "md";
  className?: string;
}) {
  return (
    <div
      className={`ui-segments ${size === "sm" ? "ui-segments-sm" : ""} ${className}`.trim()}
      role="group"
      aria-label={ariaLabel}
    >
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function FilterBar({
  ariaLabel,
  children,
}: {
  ariaLabel: string;
  children: ReactNode;
}) {
  return (
    <div className="ui-filter-bar" role="group" aria-label={ariaLabel}>
      <FunnelSimple size={17} aria-hidden="true" />
      {children}
    </div>
  );
}

export function FilterField({
  label,
  className = "",
  children,
}: {
  label: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <label className={`ui-filter-field ${className}`.trim()}>
      <span>{label}</span>
      {children}
    </label>
  );
}

export function FilterToggle({
  pressed,
  onClick,
  icon,
  children,
}: {
  pressed: boolean;
  onClick: () => void;
  icon?: ReactNode;
  children: ReactNode;
}) {
  return (
    <button type="button" className="ui-filter-toggle" aria-pressed={pressed} onClick={onClick}>
      {icon}
      {children}
    </button>
  );
}
