import { Fragment, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronDown } from "lucide-react";

export interface LauncherSelectOption {
  value: string;
  label: string;
  iconSrc?: string;
  iconClassName?: string;
  dividerBefore?: boolean;
  tone?: "danger";
}

interface LauncherSelectProps {
  value: string;
  options: LauncherSelectOption[];
  onChange: (value: string) => void;
  ariaLabel?: string;
}

const toneClass = (tone?: string) => (tone ? ` is-${tone}` : "");
const iconClass = (option?: LauncherSelectOption) =>
  ["launcher-select-icon", option?.iconClassName].filter(Boolean).join(" ");

/* A native <select> can't tint a single option, so this is a custom listbox styled
   like the launcher's controls. The popover is portaled to <body> because the launcher
   and its options row both clip overflow, then pinned below the trigger like the
   composer menu. */
export function LauncherSelect({ value, options, onChange, ariaLabel }: LauncherSelectProps) {
  const [open, setOpen] = useState(false);
  const [anchor, setAnchor] = useState<{ left: number; top: number; width: number } | null>(null);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const match = options.find((option) => option.value === value);
  const selected = match ?? options[0];

  // If the current value matches no option (e.g. a persisted choice that has since
  // been removed), the trigger would display options[0]'s label while the stored
  // value stayed orphaned — and launching would still send the stale value. Reconcile
  // to the displayed default so what's shown is what gets used.
  useEffect(() => {
    if (!match && options.length > 0 && options[0].value !== value) {
      onChange(options[0].value);
    }
  }, [match, options, value, onChange]);

  const measure = () => {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (rect) {
      // Pin the popover's top just below the trigger so it opens downward, left edge aligned.
      setAnchor({ left: rect.left, top: rect.bottom + 6, width: rect.width });
    }
  };

  useEffect(() => {
    if (!open) {
      return;
    }
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!triggerRef.current?.contains(target) && !popoverRef.current?.contains(target)) {
        setOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" || event.key === "Tab") {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    window.addEventListener("resize", measure);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("resize", measure);
    };
  }, [open]);

  return (
    <div className="launcher-select">
      <button
        ref={triggerRef}
        type="button"
        className={`control-button launcher-select-trigger${toneClass(selected?.tone)}`}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        onClick={() => {
          if (!open) {
            measure();
          }
          setOpen((prev) => !prev);
        }}
      >
        {selected?.iconSrc ? (
          <img
            className={iconClass(selected)}
            src={selected.iconSrc}
            alt=""
            aria-hidden="true"
          />
        ) : null}
        <span className="launcher-select-value">{selected?.label}</span>
        <ChevronDown size={13} className="launcher-select-chevron" aria-hidden="true" />
      </button>
      {open && anchor
        ? createPortal(
            <div
              ref={popoverRef}
              className="popover-surface launcher-select-popover"
              role="listbox"
              aria-label={ariaLabel}
              style={{ left: anchor.left, top: anchor.top, minWidth: anchor.width }}
            >
              {options.map((option) => {
                const active = option.value === value;
                return (
                  <Fragment key={option.value}>
                    {option.dividerBefore ? (
                      <div
                        className="launcher-select-separator"
                        role="separator"
                        aria-hidden="true"
                      />
                    ) : null}
                    <button
                      type="button"
                      role="option"
                      aria-selected={active}
                      className={`menu-item launcher-select-item${toneClass(option.tone)}${
                        active ? " is-active" : ""
                      }`}
                      onClick={() => {
                        setOpen(false);
                        if (option.value !== value) {
                          onChange(option.value);
                        }
                      }}
                    >
                      {option.iconSrc ? (
                        <img
                          className={iconClass(option)}
                          src={option.iconSrc}
                          alt=""
                          aria-hidden="true"
                        />
                      ) : null}
                      <span className="launcher-select-item-label">{option.label}</span>
                      {active ? (
                        <Check size={14} className="launcher-select-check" aria-hidden="true" />
                      ) : null}
                    </button>
                  </Fragment>
                );
              })}
            </div>,
            document.body,
          )
        : null}
    </div>
  );
}
