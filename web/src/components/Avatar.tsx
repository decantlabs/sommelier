import { avatarToneKey } from "../api/format";

export function Avatar({
  initials,
  person,
  size = "md",
  className = "",
}: {
  initials: string;
  person?: string;
  size?: "xs" | "sm" | "md" | "lg";
  className?: string;
}) {
  return (
    <span
      className={`ui-avatar ui-avatar-${size} ${className}`.trim()}
      data-person={person ? avatarToneKey(person) : undefined}
      aria-hidden="true"
    >
      {initials}
    </span>
  );
}
