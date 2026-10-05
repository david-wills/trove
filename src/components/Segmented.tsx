/** A row of mutually exclusive options — range pickers, tabs, toggles. */
export default function Segmented<T extends string>({
  options,
  value,
  onChange,
  small,
}: {
  options: { id: T; label: string }[];
  value: T;
  onChange: (v: T) => void;
  small?: boolean;
}) {
  return (
    <div className={`segmented ${small ? "segmented--sm" : ""}`}>
      {options.map((o) => (
        <button
          key={o.id}
          className={value === o.id ? "active" : ""}
          onClick={() => onChange(o.id)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
