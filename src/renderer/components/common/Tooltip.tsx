import { useCallback, useState } from "react";
import type { ReactNode } from "react";

export type TooltipProps = {
  content: ReactNode;
  children: ReactNode;
  /** 浮层方向：top（默认，向上弹出）/ bottom（向下弹出） */
  placement?: "top" | "bottom";
};

export const Tooltip = ({
  content,
  children,
  placement = "top",
}: TooltipProps): React.JSX.Element => {
  const [visible, setVisible] = useState(false);

  const handleMouseEnter = useCallback(() => {
    setVisible(true);
  }, []);

  const handleMouseLeave = useCallback(() => {
    setVisible(false);
  }, []);

  return (
    <span
      className="tooltip-wrapper"
      onMouseEnter={handleMouseEnter}
      onMouseLeave={handleMouseLeave}
    >
      {children}
      {visible && (
        <span className={`tooltip tooltip-${placement}`} role="tooltip">
          {content}
        </span>
      )}
    </span>
  );
};
