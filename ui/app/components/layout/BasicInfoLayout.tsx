export function BasicInfoLayout({ children }: React.PropsWithChildren) {
  return <div className="flex flex-col gap-4 md:gap-2">{children}</div>;
}

export function BasicInfoItem({ children }: React.PropsWithChildren) {
  return <div className="flex flex-col gap-0.5 md:flex-row">{children}</div>;
}

export function BasicInfoItemTitle({ children }: React.PropsWithChildren) {
  return (
    <div className="text-fg-secondary w-full flex-shrink-0 text-left text-sm md:w-32 md:py-1">
      {children}
    </div>
  );
}

export function BasicInfoItemContent({ children }: React.PropsWithChildren) {
  return (
    <div className="text-fg-primary flex flex-wrap gap-x-4 gap-y-0.5 truncate md:gap-1 md:py-1">
      {children}
    </div>
  );
}
